package worker2

import (
	"fmt"
	"maps"
	"sync"
	"time"
)

type Scheduler interface {
	Schedule(Dep, InStore) error
	Done(Dep)
}

type UnlimitedScheduler struct{}

func (ls UnlimitedScheduler) Schedule(d Dep, ins InStore) error {
	return nil
}

func (ls UnlimitedScheduler) Done(d Dep) {}

func NewLimitScheduler(limit int) *LimitScheduler {
	return &LimitScheduler{
		ch: make(chan struct{}, limit),
	}
}

type LimitScheduler struct {
	ch chan struct{}
}

func (ls *LimitScheduler) Schedule(d Dep, ins InStore) error {
	select {
	case <-d.GetCtx().Done():
		return d.GetCtx().Err()
	case ls.ch <- struct{}{}:
		return nil
	}
}

func (ls *LimitScheduler) Done(d Dep) {
	<-ls.ch
}

func NewResourceScheduler(limits map[string]float64, def map[string]float64) *ResourceScheduler {
	inuse := map[string]float64{}
	for k := range limits {
		inuse[k] = 0
	}

	return &ResourceScheduler{
		signal:   make(chan struct{}, 1),
		limits:   limits,
		inuse:    inuse,
		sessions: map[Dep]map[string]float64{},
		def:      def,
	}
}

type ResourceScheduler struct {
	m        sync.Mutex
	signal   chan struct{}
	limits   map[string]float64
	inuse    map[string]float64
	sessions map[Dep]map[string]float64
	def      map[string]float64
}

func (ls *ResourceScheduler) next() {
	select {
	case ls.signal <- struct{}{}:
	default:
	}
}

func (ls *ResourceScheduler) trySchedule(d Dep, request map[string]float64) bool {
	ls.m.Lock()
	defer ls.m.Unlock()

	for k, v := range request {
		if ls.inuse[k]+v > ls.limits[k] {
			return false
		}
	}

	for k, v := range request {
		ls.inuse[k] += v
	}

	ls.sessions[d] = maps.Clone(request)

	return true
}

func (ls *ResourceScheduler) Schedule(d Dep, ins InStore) error {
	request := d.GetRequest()

	if request == nil {
		request = ls.def
	}

	if len(request) == 0 {
		return nil
	}

	for k, rv := range request {
		lv, ok := ls.limits[k]
		if !ok {
			return fmt.Errorf("unknown resource: %v", k)
		}

		if rv > lv {
			return fmt.Errorf("requesting more resource than available, request %v got %v", rv, lv)
		}
	}

	// immediately try to schedule
	retry := time.After(0)

	for {
		select {
		case <-d.GetCtx().Done():
			return d.GetCtx().Err()
		case <-retry:
		case <-ls.signal:
		}

		success := ls.trySchedule(d, request)
		if success {
			return nil
		}
		retry = time.After(100 * time.Millisecond)
	}
}

func (ls *ResourceScheduler) Done(d Dep) {
	ls.m.Lock()
	defer ls.m.Unlock()

	s := ls.sessions[d]

	for k, v := range s {
		ls.inuse[k] -= v
	}

	delete(ls.sessions, d)

	ls.next()
}
