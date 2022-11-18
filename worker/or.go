package worker

func WaitGroupOr(wgs ...*WaitGroup) *WaitGroup {
	switch len(wgs) {
	case 0:
		panic("at least one WaitGroup required")
	case 1:
		return wgs[0]
	}

	doneCh := make(chan struct{})
	owg := &WaitGroup{}
	owg.AddSem()
	go func() {
		<-doneCh
		owg.DoneSem()
	}()

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

	return owg
}
