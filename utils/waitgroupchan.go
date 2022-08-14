package utils

import "sync"

type WaitGroupChan struct {
	ch chan struct{}
	o  sync.Once
	wg sync.WaitGroup
}

func (wg *WaitGroupChan) once() {
	wg.o.Do(func() {
		wg.ch = make(chan struct{})

		go func() {
			wg.wg.Wait()
			close(wg.ch)
			wg.o = sync.Once{}
		}()
	})
}

func (wg *WaitGroupChan) Add() {
	wg.wg.Add(1)
}

func (wg *WaitGroupChan) Sub() {
	wg.wg.Done()
}

func (wg *WaitGroupChan) Done() <-chan struct{} {
	wg.once()
	return wg.ch
}
