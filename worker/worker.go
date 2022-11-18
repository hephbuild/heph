package worker

import (
	"context"
	"fmt"
	log "github.com/sirupsen/logrus"
	"runtime/debug"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

type JobState int8

const (
	StateUnknown JobState = iota - 1
	StatePending
	StateRunning
	StateSuccess
	StateFailed
	StateSkipped
)

func (s JobState) IsDone() bool {
	return s != StateUnknown && s != StateRunning && s != StatePending
}

func (s JobState) String() string {
	switch s {
	case StatePending:
		return "pending"
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

type Job struct {
	Name  string
	ID    uint64
	Deps  *WaitGroup
	Do    func(w *Worker, ctx context.Context) error
	State JobState

	ctx    context.Context
	cancel context.CancelFunc
	doneCh chan struct{}
	err    error

	TimeStart time.Time
	TimeEnd   time.Time
}

func (j *Job) Done() {
	log.Tracef("job %v done", j.Name)
	j.doneWithState(StateSuccess)
}

func (j *Job) DoneWithErr(err error, state JobState) {
	if state == StateFailed {
		log.Errorf("%v finished with err: %v", j.Name, err)
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
}

func (j *Job) IsDone() bool {
	return j.State.IsDone()
}

type Worker struct {
	status     string
	statusm    sync.Mutex
	CurrentJob *Job
}

func (w *Worker) GetStatus() string {
	w.statusm.Lock()
	defer w.statusm.Unlock()

	return strings.Clone(w.status)
}

func (w *Worker) Status(status string) {
	w.statusm.Lock()
	defer w.statusm.Unlock()

	w.status = status
	if status != "" {
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
				w.CurrentJob = j

				err := safelyJobDo(j, w)

				j.TimeEnd = time.Now()
				w.CurrentJob = nil
				w.Status("")

				p.finalize(j, err, false)
			}
		}()
	}

	return p
}

func (p *Pool) Schedule(ctx context.Context, job *Job) *Job {
	p.wg.Add(1)

	p.m.Lock()
	defer p.m.Unlock()

	log.Tracef("Scheduling %v %v", job.Name, job.ID)

	ctx, cancel := context.WithCancel(ctx)
	go func() {
		<-p.ctx.Done()
		cancel()
	}()

	job.ID = atomic.AddUint64(&p.idc, 1)
	job.State = StatePending
	job.ctx = ctx
	job.cancel = cancel
	job.doneCh = make(chan struct{})
	if job.Deps == nil {
		job.Deps = &WaitGroup{}
	}

	p.jobs.Add(job)

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

		p.jobsCh <- job
	}()

	return job
}

func (p *Pool) finalize(job *Job, err error, skippedOnErr bool) {
	p.m.Lock()
	defer p.m.Unlock()

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
