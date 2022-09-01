package worker

import (
	"context"
	"fmt"
	log "github.com/sirupsen/logrus"
	"runtime/debug"
	"sync"
	"time"
)

type JobState int8

const (
	StateUnknown JobState = iota - 1
	StatePending
	StateRunning
	StateSuccess
	StateFailure
	StateSkipped
)

func (s JobState) String() string {
	switch s {
	case StatePending:
		return "pending"
	case StateRunning:
		return "running"
	case StateSuccess:
		return "success"
	case StateFailure:
		return "failure"
	case StateSkipped:
		return "skipped"
	case StateUnknown:
		fallthrough
	default:
		return "unknown"
	}
}

type Job struct {
	ID    string
	Do    func(w *Worker, ctx context.Context) error
	State JobState
	Deps  *WaitGroup

	ctx    context.Context
	done   bool
	doneCh chan struct{}
	err    error

	TimeStart time.Time
	TimeEnd   time.Time
}

func (j *Job) Done() {
	log.Tracef("job %v done", j.ID)
	j.doneWithState(StateSuccess)
}

func (j *Job) DoneWithErr(err error, state JobState) {
	log.Tracef("job %v done with err %v", j.ID, err)
	j.err = err
	j.doneWithState(state)
}

func (j *Job) doneWithState(state JobState) {
	j.State = state
	j.done = true
	close(j.doneCh)
}

type Worker struct {
	status     string
	CurrentJob *Job
}

func (w *Worker) GetStatus() string {
	return w.status
}

func (w *Worker) Status(status string) {
	w.status = status
	if status != "" {
		log.Info(status)
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
}

func safelyJobDo(j *Job, w *Worker) (err error) {
	defer func() {
		if rerr := recover(); rerr != nil {
			err = fmt.Errorf("panic in %v: %v => %v", j.ID, rerr, string(debug.Stack()))
		}
	}()

	return j.Do(w, j.ctx)
}

func NewPool(ctx context.Context, n int) *Pool {
	ctx, cancel := context.WithCancel(ctx)

	p := &Pool{
		ctx:    ctx,
		cancel: cancel,
		jobsCh: make(chan *Job),
		doneCh: make(chan struct{}),
		jobs:   &WaitGroup{},
	}

	go func() {
		<-ctx.Done()
		p.Stop(ctx.Err())
	}()

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

type ScheduleOptions struct {
	OnSchedule func()
}

func (p *Pool) Schedule(job *Job) *Job {
	return p.ScheduleWith(ScheduleOptions{}, job)
}

func (p *Pool) ScheduleWith(opt ScheduleOptions, job *Job) *Job {
	p.wg.Add(1)

	p.m.Lock()
	defer p.m.Unlock()

	if j := p.jobs.Job(job.ID); j != nil {
		p.wg.Done()
		return j
	}

	log.Tracef("Scheduling %v", job.ID)

	job.State = StatePending
	job.ctx = p.ctx
	job.doneCh = make(chan struct{})
	if job.Deps == nil {
		job.Deps = &WaitGroup{}
	}

	p.jobs.Add(job)

	if f := opt.OnSchedule; f != nil {
		f()
	}

	go func() {
		select {
		case <-job.ctx.Done():
			p.finalize(job, job.ctx.Err(), true)
			return
		case <-job.Deps.Done():
			if err := job.Deps.Err(); err != nil {
				p.finalize(job, err, true)
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
		log.Debugf("finalize job: %v %v", job.ID, job.State.String())
	} else {
		if skippedOnErr {
			job.DoneWithErr(err, StateSkipped)
		} else {
			job.DoneWithErr(err, StateFailure)
		}
		log.Debugf("finalize job err: %v %v: %v", job.ID, job.State.String(), err)

		go p.Stop(fmt.Errorf("%v: %v", job.ID, err))
	}

	p.wg.Done()
}

func (p *Pool) Job(id string) *Job {
	p.m.Lock()
	defer p.m.Unlock()

	return p.jobs.Job(id)
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
