package worker

import (
	"errors"
	"fmt"
	"sync"
	"sync/atomic"
)

type WaitGroup struct {
	m      sync.Mutex
	jobs   []*Job
	doneCh chan struct{}
	err    error
	cond   *sync.Cond
	oSetup sync.Once
	oDone  sync.Once
}

func (wg *WaitGroup) Add(job *Job) {
	if job == nil {
		panic("job cannot be nil")
	}

	wg.m.Lock()
	defer wg.m.Unlock()

	if wg.Job(job.ID) != nil {
		return
	}

	go func() {
		<-job.doneCh
		wg.broadcast()
	}()

	wg.jobs = append(wg.jobs, job)
}

func (wg *WaitGroup) AddFrom(deps *WaitGroup) {
	for _, job := range deps.Jobs() {
		wg.Add(job)
	}
}

func (wg *WaitGroup) Job(id string) *Job {
	for _, job := range wg.jobs[:] {
		if job.ID == id {
			return job
		}
	}

	return nil
}

func (wg *WaitGroup) broadcast() {
	wg.m.Lock()
	defer wg.m.Unlock()

	if wg.cond == nil {
		return
	}

	wg.cond.Broadcast()
}

func (wg *WaitGroup) wait() {
	wg.cond.L.Lock()
	var err error
	for {
		err = wg.keepWaiting()
		if !errors.Is(err, ErrPending) {
			break
		}

		wg.cond.Wait()
	}

	if wg.err == nil {
		wg.err = err
	}
	close(wg.doneCh)

	wg.cond.L.Unlock()
}

func (wg *WaitGroup) Done() <-chan struct{} {
	wg.m.Lock()
	defer wg.m.Unlock()

	wg.oSetup.Do(func() {
		wg.cond = sync.NewCond(&wg.m)
	})

	wg.oDone.Do(func() {
		wg.doneCh = make(chan struct{})
		go wg.wait()
	})

	return wg.doneCh
}

func (wg *WaitGroup) Err() error {
	return wg.err
}

func (wg *WaitGroup) Jobs() []*Job {
	wg.m.Lock()
	defer wg.m.Unlock()

	return wg.jobs[:]
}

func (wg *WaitGroup) walkerTransitiveCount(c *uint64, m map[string]struct{}, f func(j *Job) bool) {
	for _, job := range wg.jobs[:] {
		if _, ok := m[job.ID]; ok {
			continue
		}
		m[job.ID] = struct{}{}

		if f(job) {
			atomic.AddUint64(c, 1)
		}

		job.Deps.walkerTransitiveCount(c, m, f)
	}
}

func (wg *WaitGroup) transitiveCount(f func(j *Job) bool) uint64 {
	var c uint64
	m := map[string]struct{}{}
	wg.walkerTransitiveCount(&c, m, f)

	return c
}

func (wg *WaitGroup) TransitiveJobCount() uint64 {
	return wg.transitiveCount(func(j *Job) bool {
		return true
	})
}

func (wg *WaitGroup) TransitiveSuccessCount() uint64 {
	return wg.transitiveCount(func(j *Job) bool {
		return j.State == StateSuccess
	})
}

var ErrPending = fmt.Errorf("pending")

// keepWaiting returns ErrPending if it should keep waiting, nil represents no jobs error
func (wg *WaitGroup) keepWaiting() error {
	for _, job := range wg.jobs[:] {
		if !job.done {
			return fmt.Errorf("%v is %w", job.ID, ErrPending)
		}

		if job.State == StateSuccess {
			continue
		}

		if job.State == StateRunning {
			continue
		}

		if job.State == StatePending {
			return fmt.Errorf("%v is %w", job.ID, ErrPending)
		}

		jerr := job.err
		if jerr != nil {
			return fmt.Errorf("%v is %v: %v", job.ID, job.State.String(), jerr)
		}

		return fmt.Errorf("%v is %v", job.ID, job.State.String())
	}

	return nil
}
