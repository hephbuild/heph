package worker

import (
	"errors"
	"fmt"
	"sync"
	"sync/atomic"
)

type WaitGroup struct {
	m      sync.RWMutex
	wgs    []*WaitGroup
	jobs   []*Job
	jobsm  map[string]*Job
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

	if wg.jobsm == nil {
		wg.jobsm = map[string]*Job{}
	}

	if wg.job(job.ID, false) != nil {
		return
	}

	go func() {
		<-job.doneCh
		wg.broadcast()
	}()

	wg.jobsm[job.ID] = job
	wg.jobs = append(wg.jobs, job)
}

func (wg *WaitGroup) AddChild(child *WaitGroup) {
	wg.m.Lock()
	defer wg.m.Unlock()

	go func() {
		<-child.Done()
		wg.broadcast()
	}()

	wg.wgs = append(wg.wgs, child)
}

func (wg *WaitGroup) Job(id string, transitive bool) *Job {
	wg.m.RLock()
	defer wg.m.RUnlock()

	return wg.job(id, transitive)
}

func (wg *WaitGroup) job(id string, transitive bool) *Job {
	if wg.jobsm != nil {
		if j := wg.jobsm[id]; j != nil {
			return j
		}
	}

	if transitive {
		for _, wg := range wg.wgs {
			if job := wg.Job(id, transitive); job != nil {
				return job
			}
		}
	}

	return nil
}

func (wg *WaitGroup) broadcast() {
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
	wg.oSetup.Do(func() {
		wg.cond = sync.NewCond(&sync.Mutex{})
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

	for _, wg := range wg.wgs {
		wg.walkerTransitiveCount(c, m, f)
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
	for _, wg := range wg.wgs[:] {
		err := wg.keepWaiting()
		if err != nil {
			return err
		}
	}

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
