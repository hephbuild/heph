package worker2

import (
	"context"
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

func ErrorHook() (Hook, <-chan error) {
	ch := make(chan error, 1)
	return func(event Event) {
		switch event := event.(type) {
		case EventCompleted:
			ch <- event.Error
			close(ch)
		case EventSkipped:
			ch <- event.Error
			close(ch)
		}
	}, ch
}

func LogHook() Hook {
	return func(event Event) {
		if event, ok := event.(EventWithExecution); ok {
			fmt.Printf("%v: %T %+v\n", event.getExecution().Dep.GetName(), event, event)
		} else {
			fmt.Printf("%T %+v\n", event, event)
		}
	}
}

type StageHook struct {
	OnScheduled func(Dep) context.Context
	// OnWaiting
	OnQueued func(Dep) context.Context
	OnStart  func(Dep) context.Context
	OnEnd    func(Dep) context.Context
}

func (h StageHook) Hook() Hook {
	return func(event1 Event) {
		event, ok := event1.(EventWithExecution)
		if !ok {
			return
		}

		exec := event.getExecution()

		ctx := h.run(exec)
		if ctx != nil {
			exec.Dep.SetCtx(ctx)
		}
	}
}

func (h StageHook) run(exec *Execution) context.Context {
	state := exec.State
	dep := exec.Dep

	switch state {
	case ExecStateScheduled:
		if h.OnScheduled != nil {
			return h.OnScheduled(dep)
		}
	case ExecStateQueued:
		if h.OnQueued != nil {
			return h.OnQueued(dep)
		}
	case ExecStateRunning:
		if h.OnStart != nil {
			return h.OnStart(dep)
		}
	default:
		if state.IsFinal() {
			if h.OnEnd != nil {
				return h.OnEnd(dep)
			}
		}
	}
	return nil
}
