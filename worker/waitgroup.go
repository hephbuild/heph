package worker

import (
	"errors"
	"fmt"
	"go.uber.org/multierr"
	"sync"
	"sync/atomic"
)

type WaitGroup struct {
	m      sync.RWMutex
	wgs    []*WaitGroup
	jobs   []*Job
	doneCh chan struct{}
	done   bool
	err    error
	cond   *sync.Cond
	oSetup sync.Once
	oDone  sync.Once
	sem    int64

	failfast bool
}

func (wg *WaitGroup) Add(job *Job) {
	if job == nil {
		panic("job cannot be nil")
	}

	wg.m.Lock()
	defer wg.m.Unlock()

	go func() {
		<-job.doneCh
		wg.handleUnitDone()
	}()

	wg.jobs = append(wg.jobs, job)
}

func (wg *WaitGroup) AddChild(child *WaitGroup) {
	if child == nil {
		panic("child cannot be nil")
	}

	wg.m.Lock()
	defer wg.m.Unlock()

	go func() {
		<-child.Done()
		wg.handleUnitDone()
	}()

	wg.wgs = append(wg.wgs, child)
}

func (wg *WaitGroup) handleUnitDone() {
	wg.broadcast()
}

func (wg *WaitGroup) AddSem() {
	atomic.AddInt64(&wg.sem, 1)
}

func (wg *WaitGroup) DoneSem() {
	if atomic.AddInt64(&wg.sem, -1) < 0 {
		panic("too many calls to DoneSem")
	}
	wg.broadcast()
}

func (wg *WaitGroup) Jobs() []*Job {
	jobs := wg.jobs[:]
	for _, wg := range wg.wgs[:] {
		jobs = append(jobs, wg.Jobs()...)
	}

	return jobs
}

func (wg *WaitGroup) broadcast() {
	if wg.cond == nil {
		return
	}

	wg.cond.L.Lock()
	defer wg.cond.L.Unlock()

	wg.cond.Broadcast()
}

func (wg *WaitGroup) wait() {
	wg.cond.L.Lock()

	waitm := map[*WaitGroup]error{}

	var err error
	for {
		err = wg.keepWaiting(wg.failfast, waitm)
		if !errors.Is(err, ErrPending) {
			break
		}

		wg.cond.Wait()
	}

	if wg.err == nil {
		wg.err = err
	}
	wg.done = true
	close(wg.doneCh)

	wg.cond.L.Unlock()
}

func (wg *WaitGroup) Done() <-chan struct{} {
	wg.oSetup.Do(func() {
		wg.cond = sync.NewCond(&wg.m)
	})

	wg.oDone.Do(func() {
		wg.doneCh = make(chan struct{})
		go wg.wait()
	})

	return wg.doneCh
}

func (wg *WaitGroup) IsDone() bool {
	return wg.done
}

func (wg *WaitGroup) Err() error {
	return wg.err
}

func (wg *WaitGroup) walkerTransitiveDo(mj map[uint64]struct{}, mwg map[*WaitGroup]struct{}, f func(j *Job)) {
	if _, ok := mwg[wg]; ok {
		return
	}
	mwg[wg] = struct{}{}

	for _, job := range wg.jobs {
		if _, ok := mj[job.ID]; ok {
			continue
		}
		mj[job.ID] = struct{}{}

		f(job)

		job.Deps.walkerTransitiveDo(mj, mwg, f)
	}

	for _, wg := range wg.wgs {
		wg.walkerTransitiveDo(mj, mwg, f)
	}
}

func (wg *WaitGroup) TransitiveDo(f func(j *Job)) {
	mj := map[uint64]struct{}{}
	mwg := map[*WaitGroup]struct{}{}
	wg.walkerTransitiveDo(mj, mwg, f)
}

type WaitGroupStats struct {
	All     uint64
	Done    uint64
	Success uint64
	Failed  uint64
	Skipped uint64
}

func (wg *WaitGroup) TransitiveCount() WaitGroupStats {
	s := WaitGroupStats{}

	wg.TransitiveDo(func(j *Job) {
		atomic.AddUint64(&s.All, 1)

		if j.IsDone() {
			atomic.AddUint64(&s.Done, 1)
		}

		if j.State == StateSuccess {
			atomic.AddUint64(&s.Success, 1)
		}

		if j.State == StateFailed {
			atomic.AddUint64(&s.Failed, 1)
		}

		if j.State == StateSkipped {
			atomic.AddUint64(&s.Skipped, 1)
		}
	})

	return s
}

var ErrPending = errors.New("pending")

// keepWaiting returns ErrPending if it should keep waiting, nil represents no jobs error
func (wg *WaitGroup) keepWaiting(failFast bool, m map[*WaitGroup]error) (rerr error) {
	if err, ok := m[wg]; ok {
		return err
	}
	defer func() {
		if !errors.Is(rerr, ErrPending) {
			m[wg] = rerr
		}
	}()

	if atomic.LoadInt64(&wg.sem) > 0 {
		return fmt.Errorf("sem is > 0: %w", ErrPending)
	}

	var errs []error

	for _, wg := range wg.wgs[:] {
		err := wg.keepWaiting(failFast, m)
		if err != nil {
			if failFast {
				return err
			} else {
				errs = append(errs, err)
			}
		}
	}

	allDone := true

	for _, job := range wg.jobs[:] {
		state := job.State

		if StateIsDone(state) {
			if state == StateSuccess {
				continue
			}

			jerr := fmt.Errorf("2 %v is %v: %w", job.Name, state.String(), job.err)

			if failFast {
				return jerr
			} else {
				errs = append(errs, jerr)
			}
		} else {
			allDone = false

			return fmt.Errorf("1 %v is %w", job.Name, ErrPending)
		}
	}

	if !allDone {
		return nil
	}

	return multierr.Combine(errs...)
}
