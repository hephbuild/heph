package worker

import (
	"context"
	"fmt"
	log "github.com/sirupsen/logrus"
	"runtime/debug"
	"sync"
	"sync/atomic"
	"time"
)

type Job struct {
	ID   string
	Do   func(w *Worker, ctx context.Context) error
	Wait func(ctx context.Context)

	ctx       context.Context
	TimeStart time.Time
	TimeEnd   time.Time
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
	Err     error

	JobCount  uint64
	DoneCount uint64

	doneCh chan struct{}
	mDone  sync.Mutex
	o      sync.Once

	jobsCh  chan *Job
	wg      sync.WaitGroup
	stopped bool
	mErr    sync.Mutex
	jobs    map[string]*Job
	mJobs   sync.Mutex
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
		jobs:   map[string]*Job{},
	}

	for i := 0; i < n; i++ {
		w := &Worker{}
		p.Workers = append(p.Workers, w)

		go func() {
			for j := range p.jobsCh {
				if p.stopped {
					// Drain chan
					p.wg.Done()
					continue
				}

				j.TimeStart = time.Now()
				w.CurrentJob = j

				err := safelyJobDo(j, w)
				if err != nil {
					log.Errorf("%v failed: %v", j.ID, err)
				}

				j.TimeEnd = time.Now()
				w.CurrentJob = nil
				w.Status("")

				p.mErr.Lock()
				if err != nil && p.Err == nil {
					p.Err = err
					p.mErr.Unlock()
					go p.Stop()
				} else {
					p.mErr.Unlock()
				}

				if err == nil {
					atomic.AddUint64(&p.DoneCount, 1)
				}

				p.wg.Done()
			}
		}()
	}

	return p
}

type ScheduleOptions struct {
	OnSchedule func()
}

func (p *Pool) Schedule(job *Job) {
	p.ScheduleWith(ScheduleOptions{}, job)
}

func (p *Pool) ScheduleWith(opt ScheduleOptions, job *Job) {
	p.wg.Add(1)

	p.mJobs.Lock()
	defer p.mJobs.Unlock()

	if _, ok := p.jobs[job.ID]; ok {
		p.wg.Done()
		return
	}

	job.ctx = p.ctx
	atomic.AddUint64(&p.JobCount, 1)

	p.jobs[job.ID] = job

	if f := opt.OnSchedule; f != nil {
		f()
	}

	go func() {
		if job.Wait != nil {
			job.Wait(job.ctx)
		}

		if p.stopped {
			p.wg.Done()
			return
		}

		p.jobsCh <- job
	}()
}

func (p *Pool) Done() <-chan struct{} {
	p.mDone.Lock()
	defer p.mDone.Unlock()

	p.o.Do(func() {
		p.doneCh = make(chan struct{})

		go func() {
			p.wg.Wait()

			p.mDone.Lock()
			defer p.mDone.Unlock()

			close(p.doneCh)
			p.o = sync.Once{}
		}()
	})

	return p.doneCh
}

func (p *Pool) Stop() {
	if p.stopped {
		return
	}
	p.stopped = true

	p.cancel()
}
