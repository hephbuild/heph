package worker

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/log/log"
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

	ctx    context.Context
	cancel context.CancelFunc
	doneCh chan struct{}
	err    error

	TimeScheduled time.Time
	TimeQueued    time.Time
	TimeStart     time.Time
	TimeEnd       time.Time

	m sync.Mutex
}

func (j *Job) RunHook() {
	ctx := j.ctx
	if h := j.Hook; h != nil {
		jctx := h.Run(j)
		if jctx != nil {
			ctx = jctx
		}
	}
	j.ctx = ctx
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
}

func (j *Job) IsDone() bool {
	return j.State.IsDone()
}

type Status interface {
	String(term bool) string
}

func StringStatus(status string) Status {
	return stringStatus(status)
}

type stringStatus string

func (s stringStatus) String(bool) string {
	return string(s)
}

type Worker struct {
	status     Status
	statusm    sync.Mutex
	CurrentJob *Job
}

func (w *Worker) GetStatus() Status {
	if w.status == nil {
		return StringStatus("")
	}

	return w.status
}

func (w *Worker) Status(status Status) {
	w.status = status
	if status := status.String(false); status != "" {
		log.Debug(status)
	}
}

type Pool struct {
	ctx     context.Context
	cancel  func()
	Workers []*Worker

	doneCh chan struct{}
	o      sync.Once
	cond   sync.Cond

	jobsCh  chan *Job
	wg      sync.WaitGroup
	stopped bool
	stopErr error
	jobs    *WaitGroup
	m       sync.Mutex
	idc     uint64
}

func safelyJobDo(j *Job, w *Worker) (err error) {
	defer func() {
		if rerr := recover(); rerr != nil {
			err = fmt.Errorf("panic in %v: %v => %v", j.Name, rerr, string(debug.Stack()))
		}
	}()

	return j.Do(w, j.ctx)
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

				j.TimeStart = time.Now()
				j.State = StateRunning
				w.CurrentJob = j

				j.RunHook()
				err := safelyJobDo(j, w)

				j.TimeEnd = time.Now()
				w.CurrentJob = nil
				w.Status(StringStatus(""))

				p.finalize(j, err, false)
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
	job.ctx = ctx
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
