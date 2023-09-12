package worker

import (
	"context"
	"fmt"
	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/utils/xpanic"
	"github.com/muesli/termenv"
	"io"
	"runtime/debug"
	"sync"
	"sync/atomic"
	"time"
)

type JobState int8

const (
	StateUnknown JobState = iota
	StateScheduled
	StateQueued
	StateRunning
	StateSuccess
	StateFailed
	StateSkipped
	StateSuspended
)

func (s JobState) IsDone() bool {
	return s == StateSuccess || s == StateFailed || s == StateSkipped
}

func (s JobState) String() string {
	switch s {
	case StateScheduled:
		return "scheduled"
	case StateQueued:
		return "queued"
	case StateRunning:
		return "running"
	case StateSuccess:
		return "success"
	case StateFailed:
		return "failed"
	case StateSkipped:
		return "skipped"
	case StateSuspended:
		return "suspended"
	case StateUnknown:
		fallthrough
	default:
		return "unknown"
	}
}

type Hook interface {
	Run(*Job) context.Context
}

type StageHook struct {
	OnScheduled func(*Job) context.Context
	OnQueued    func(*Job) context.Context
	OnStart     func(*Job) context.Context
	OnEnd       func(*Job) context.Context
}

func (h StageHook) Run(j *Job) context.Context {
	switch j.State {
	case StateScheduled:
		if h.OnScheduled != nil {
			return h.OnScheduled(j)
		}
	case StateQueued:
		if h.OnQueued != nil {
			return h.OnQueued(j)
		}
	case StateRunning:
		if h.OnStart != nil {
			return h.OnStart(j)
		}
	default:
		if j.IsDone() {
			if h.OnEnd != nil {
				return h.OnEnd(j)
			}
		}
	}
	return nil
}

type Job struct {
	Name  string
	ID    uint64
	Deps  *WaitGroup
	Do    func(w *Worker, ctx context.Context) error
	State JobState
	Hook  Hook

	status status.Statuser

	ctx    context.Context
	cancel context.CancelFunc
	doneCh chan struct{}
	err    error

	runCh       chan error
	suspendCh   chan struct{}
	suspendedCh chan struct{}
	resumeCh    chan struct{}

	TimeScheduled time.Time
	TimeQueued    time.Time
	TimeStart     time.Time
	TimeEnd       time.Time

	m sync.Mutex
}

var stringRenderer = lipgloss.NewRenderer(io.Discard, termenv.WithColorCache(true))

func (j *Job) Status(status status.Statuser) {
	j.status = status
	if s := status.String(stringRenderer); s != "" {
		log.Debug(s)
	}
}

func (j *Job) GetStatus() status.Statuser {
	s := j.status
	if s == nil {
		return status.Clear()
	}

	return s
}

func (j *Job) Interactive() bool {
	return true
}

func (j *Job) prepare() (<-chan error, <-chan struct{}, bool) {
	j.m.Lock()
	defer j.m.Unlock()

	isInit := j.runCh == nil

	if j.runCh == nil {
		j.runCh = make(chan error)
	}

	j.suspendCh = make(chan struct{})

	return j.runCh, j.suspendCh, isInit
}

func (j *Job) suspend() chan struct{} {
	j.m.Lock()

	if j.resumeCh != nil {
		panic("pause on paused job")
	}

	j.resumeCh = make(chan struct{})
	j.suspendedCh = make(chan struct{})

	close(j.suspendCh)

	return j.resumeCh
}

func (j *Job) suspendAck(w *Worker) {
	defer j.m.Unlock()

	w.CurrentJob = nil
	j.State = StateSuspended

	close(j.suspendedCh)
}

func (j *Job) resume(w *Worker) {
	j.m.Lock()
	defer j.m.Unlock()

	resumeCh := j.resumeCh

	if resumeCh == nil {
		panic("resume on unpaused job")
	}

	j.resumeCh = nil
	w.CurrentJob = j
	j.State = StateRunning

	close(resumeCh)
}

func (j *Job) run(w *Worker) {
	j.ctx = status.ContextWithHandler(j.ctx, j)

	j.TimeStart = time.Now()
	j.State = StateRunning
	w.CurrentJob = j

	j.RunHook()
	err := xpanic.Recover(func() error {
		return j.Do(w, j.ctx)
	}, xpanic.Wrap(func(err any) error {
		return fmt.Errorf("panic in %v: %v => %v", j.Name, err, string(debug.Stack()))
	}))

	j.TimeEnd = time.Now()
	w.CurrentJob = nil

	j.runCh <- err
}

func (j *Job) RunHook() {
	if h := j.Hook; h != nil {
		ctx := j.ctx
		jctx := h.Run(j)
		if jctx != nil {
			ctx = jctx
		}
		j.ctx = ctx
	}
}

func (j *Job) Ctx() context.Context {
	return j.ctx
}

func (j *Job) Err() error {
	return j.err
}

func (j *Job) Wait() <-chan struct{} {
	return j.doneCh
}

func (j *Job) Done() {
	j.m.Lock()
	defer j.m.Unlock()

	j.doneWithState(StateSuccess)
}

func (j *Job) DoneWithErr(err error, state JobState) {
	j.m.Lock()
	defer j.m.Unlock()

	if state == StateFailed {
		log.Errorf("%v finished with err: %v", j.Name, err)
	} else {
		log.Tracef("%v finished with %v err: %v", j.Name, state, err)
	}

	jerr := JobError{
		ID:    j.ID,
		Name:  j.Name,
		State: state,
		Err:   err,
	}
	j.err = jerr
	j.doneWithState(state)
}

func (j *Job) doneWithState(state JobState) {
	j.State = state
	close(j.doneCh)
	j.RunHook()
	//j.cancel()
}

func (j *Job) IsDone() bool {
	return j.State.IsDone()
}

type Worker struct {
	CurrentJob *Job
}

func (w *Worker) GetStatus() status.Statuser {
	j := w.CurrentJob
	if j == nil {
		return status.Clear()
	}

	return j.GetStatus()
}

type Pool struct {
	ctx     context.Context
	cancel  func()
	Workers []*Worker

	doneCh chan struct{}
	o      sync.Once

	jobsCh  chan *Job
	wg      sync.WaitGroup
	stopped bool
	stopErr error
	jobs    *WaitGroup
	m       sync.Mutex
	idc     uint64
}

func NewPool(n int) *Pool {
	ctx, cancel := context.WithCancel(context.Background())

	p := &Pool{
		ctx:    ctx,
		cancel: cancel,
		jobsCh: make(chan *Job),
		doneCh: make(chan struct{}),
		jobs:   &WaitGroup{},
	}

	for i := 0; i < n; i++ {
		w := &Worker{}
		p.Workers = append(p.Workers, w)

		go func() {
			for j := range p.jobsCh {
				if p.stopped {
					// Drain chan
					p.finalize(j, fmt.Errorf("pool stopped"), true)
					continue
				}

				runCh, suspendCh, isInit := j.prepare()

				if isInit {
					go func() {
						j.run(w)
					}()
				} else {
					j.resume(w)
				}

				select {
				case err := <-runCh:
					p.finalize(j, err, false)
				case <-suspendCh:
					j.suspendAck(w)
				}
			}
		}()
	}

	return p
}

func (p *Pool) Schedule(ctx context.Context, job *Job) *Job {
	p.wg.Add(1)

	ctx, cancel := context.WithCancel(ctx)
	go func() {
		<-p.ctx.Done()
		cancel()
	}()

	job.ID = atomic.AddUint64(&p.idc, 1)
	job.State = StateScheduled
	job.TimeScheduled = time.Now()
	job.ctx = ContextWithPoolJob(ctx, p, job)
	job.cancel = cancel
	job.doneCh = make(chan struct{})
	if job.Deps == nil {
		job.Deps = &WaitGroup{}
	}

	log.Tracef("Scheduling %v %v", job.Name, job.ID)

	p.jobs.Add(job)
	job.RunHook()

	go func() {
		select {
		case <-job.ctx.Done():
			p.finalize(job, job.ctx.Err(), true)
			return
		case <-job.Deps.Done():
			if err := job.Deps.Err(); err != nil {
				p.finalize(job, CollectRootErrors(err), true)
				return
			}
		}

		job.State = StateQueued
		job.TimeQueued = time.Now()
		job.RunHook()

		p.jobsCh <- job
	}()

	return job
}

func (p *Pool) finalize(job *Job, err error, skippedOnErr bool) {
	if err == nil {
		job.Done()
		//log.Debugf("finalize job: %v %v", job.Name, job.State.String())
	} else {
		if skippedOnErr {
			job.DoneWithErr(err, StateSkipped)
		} else {
			job.DoneWithErr(err, StateFailed)
		}
		//log.Debugf("finalize job err: %v %v: %v", job.Name, job.State.String(), err)
	}

	p.wg.Done()
}

func (p *Pool) Jobs() []*Job {
	return p.jobs.jobs[:]
}

func (p *Pool) IsDone() bool {
	return p.jobs.IsDone()
}

func (p *Pool) Done() <-chan struct{} {
	p.m.Lock()
	defer p.m.Unlock()

	p.o.Do(func() {
		p.doneCh = make(chan struct{})

		go func() {
			p.wg.Wait()

			p.m.Lock()
			defer p.m.Unlock()

			close(p.doneCh)
			p.o = sync.Once{}
		}()
	})

	return p.doneCh
}

func (p *Pool) Err() error {
	return p.stopErr
}

func (p *Pool) Stop(err error) {
	p.m.Lock()
	defer p.m.Unlock()

	if p.stopped {
		return
	}

	p.stopped = true
	p.stopErr = err

	p.cancel()
}
