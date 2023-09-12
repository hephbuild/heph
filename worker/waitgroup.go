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
	err    error
	cond   *sync.Cond
	oSetup sync.Once
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
	v := atomic.AddInt64(&wg.sem, -1)
	if v < 0 {
		panic("too many calls to DoneSem")
	} else if v == 0 {
		wg.handleUnitDone()
	}
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
	defer wg.cond.L.Unlock()

	var err error
	for {
		err = wg.keepWaiting(wg.failfast)
		if !errors.Is(err, ErrPending) {
			break
		}

		wg.cond.Wait()
	}

	if wg.err == nil {
		wg.err = err
	}
	close(wg.doneCh)
}

func (wg *WaitGroup) Done() <-chan struct{} {
	wg.oSetup.Do(func() {
		wg.cond = sync.NewCond(&wg.m)
		wg.doneCh = make(chan struct{})
		go wg.wait()
	})

	return wg.doneCh
}

func (wg *WaitGroup) IsDone() bool {
	select {
	case <-wg.Done():
		return true
	default:
		return false
	}
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
	All       uint64
	Done      uint64
	Success   uint64
	Failed    uint64
	Skipped   uint64
	Suspended uint64
}

func (wg *WaitGroup) TransitiveCount() WaitGroupStats {
	s := WaitGroupStats{}

	wg.TransitiveDo(func(j *Job) {
		atomic.AddUint64(&s.All, 1)

		if j.IsDone() {
			atomic.AddUint64(&s.Done, 1)
		}

		switch j.State {
		case StateSuccess:
			atomic.AddUint64(&s.Success, 1)
		case StateFailed:
			atomic.AddUint64(&s.Failed, 1)
		case StateSkipped:
			atomic.AddUint64(&s.Skipped, 1)
		case StateSuspended:
			atomic.AddUint64(&s.Suspended, 1)
		}
	})

	return s
}

var ErrPending = errors.New("pending")

var ErrSemPending = fmt.Errorf("sem is > 0: %w", ErrPending)

// keepWaiting returns ErrPending if it should keep waiting, nil represents no jobs error
func (wg *WaitGroup) keepWaiting(failFast bool) error {
	if atomic.LoadInt64(&wg.sem) > 0 {
		return ErrSemPending
	}

	var errs []error
	jerrs := map[uint64]JobError{}
	addErr := func(err error) {
		var jerr JobError
		if errors.As(err, &jerr) {
			jerrs[jerr.ID] = jerr
		} else {
			errs = append(errs, err)
		}
	}

	for _, wg := range wg.wgs {
		select {
		case <-wg.Done():
			err := wg.Err()
			if err != nil {
				if failFast {
					return err
				} else {
					for _, err := range multierr.Errors(err) {
						addErr(err)
					}
				}
			}

		default:
			return ErrPending
		}
	}

	for _, job := range wg.jobs {
		state := job.State

		if state.IsDone() {
			if state == StateSuccess {
				continue
			}

			if failFast {
				return job.err
			} else {
				addErr(job.err)
			}
		} else {
			return ErrPending
		}
	}

	for _, err := range jerrs {
		errs = append(errs, err)
	}

	return multierr.Combine(errs...)
}
