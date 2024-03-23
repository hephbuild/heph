package worker2

type Scheduler interface {
	Schedule(Dep, InStore) error
	Done(Dep)
}

type UnlimitedScheduler struct {
	ch chan struct{}
}

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
