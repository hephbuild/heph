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
	doneCh chan struct{}
	done   bool
	err    error
	cond   *sync.Cond
	oSetup sync.Once
	oDone  sync.Once
	sem    int64
}

func (wg *WaitGroup) Add(job *Job) {
	if job == nil {
		panic("job cannot be nil")
	}

	wg.m.Lock()
	defer wg.m.Unlock()

	go func() {
		<-job.doneCh
		wg.handleDone(job.err)
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
		wg.handleDone(child.Err())
	}()

	wg.wgs = append(wg.wgs, child)
}

func (wg *WaitGroup) handleDone(err error) {
	if err != nil {
		wg.err = err
	}
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

func (wg *WaitGroup) walkerTransitiveCount(mj map[uint64]struct{}, mwg map[*WaitGroup]struct{}, fs ...func(j *Job)) {
	if _, ok := mwg[wg]; ok {
		return
	}
	mwg[wg] = struct{}{}

	for _, job := range wg.jobs[:] {
		if _, ok := mj[job.ID]; ok {
			continue
		}
		mj[job.ID] = struct{}{}

		for _, f := range fs {
			f(job)
		}

		job.Deps.walkerTransitiveCount(mj, mwg, fs...)
	}

	for _, wg := range wg.wgs {
		wg.walkerTransitiveCount(mj, mwg, fs...)
	}
}

func (wg *WaitGroup) TransitiveCount() (uint64, uint64) {
	var all, success uint64

	mj := map[uint64]struct{}{}
	mwg := map[*WaitGroup]struct{}{}
	wg.walkerTransitiveCount(mj, mwg,
		func(j *Job) {
			atomic.AddUint64(&all, 1)
		},
		func(j *Job) {
			if j.State == StateSuccess {
				atomic.AddUint64(&success, 1)
			}
		},
	)

	return all, success
}

var ErrPending = errors.New("pending")

// keepWaiting returns ErrPending if it should keep waiting, nil represents no jobs error
func (wg *WaitGroup) keepWaiting() error {
	if wg.done {
		return nil
	}

	if wg.err != nil {
		return wg.err
	}

	if atomic.LoadInt64(&wg.sem) > 0 {
		return fmt.Errorf("sem is > 0: %w", ErrPending)
	}

	for _, wg := range wg.wgs[:] {
		err := wg.keepWaiting()
		if err != nil {
			return err
		}
	}

	for _, job := range wg.jobs[:] {
		job := job

		if !job.done {
			return fmt.Errorf("%v is %w", job.Name, ErrPending)
		}

		if job.State == StateSuccess {
			continue
		}

		if job.State == StateRunning {
			continue
		}

		if job.State == StatePending {
			return fmt.Errorf("%v is %w", job.Name, ErrPending)
		}

		jerr := job.err
		if jerr != nil {
			return fmt.Errorf("%v is %v: %w", job.Name, job.State.String(), jerr)
		}

		return fmt.Errorf("%v is %v", job.Name, job.State.String())
	}

	return nil
}
