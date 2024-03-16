package worker2

type Hook func(Event)

func OutputHook() (Hook, <-chan Value) {
	ch := make(chan Value, 1)
	return func(event Event) {
		switch event := event.(type) {
		case EventCompleted:
			ch <- event.Output
			close(ch)
		case EventSkipped:
			close(ch)
		}
	}, ch
}
