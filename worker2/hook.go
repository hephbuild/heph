package worker2

import (
	"errors"
	"fmt"
)

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

var ErrSkipped = errors.New("skipped")

func ErrorHook() (Hook, <-chan error) {
	ch := make(chan error, 1)
	return func(event Event) {
		switch event := event.(type) {
		case EventCompleted:
			ch <- event.Error
			close(ch)
		case EventSkipped:
			ch <- ErrSkipped
			close(ch)
		}
	}, ch
}

func LogHook() Hook {
	return func(event Event) {
		if event, ok := event.(WithExecution); ok {
			fmt.Printf("%v: %T %+v\n", event.getExecution().Dep.GetID(), event, event)
		} else {
			fmt.Printf("%T %+v\n", event, event)
		}
	}
}
