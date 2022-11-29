package worker

func WaitGroupOr(wgs ...*WaitGroup) *WaitGroup {
	switch len(wgs) {
	case 0:
		panic("at least one WaitGroup required")
	case 1:
		return wgs[0]
	}

	doneCh := make(chan struct{})

	for _, wg := range wgs {
		wg := wg

		go func() {
			select {
			case <-doneCh:
			case <-wg.Done():
				close(doneCh)
			}
		}()
	}

	return WaitGroupChan(doneCh)
}

func WaitGroupJob(j *Job) *WaitGroup {
	wg := &WaitGroup{}
	wg.Add(j)

	return wg
}

func WaitGroupChan[T any](ch <-chan T) *WaitGroup {
	wg := &WaitGroup{}
	wg.AddSem()
	go func() {
		<-ch
		wg.DoneSem()
	}()

	return wg
}
