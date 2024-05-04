package worker2

import "time"

type Event any

type WithExecution interface {
	getExecution() *Execution
}

type EventCompleted struct {
	At        time.Time
	Execution *Execution
	Output    Value
	Error     error
}

func (e EventCompleted) getExecution() *Execution {
	return e.Execution
}

type EventScheduled struct {
	At        time.Time
	Execution *Execution
}

func (e EventScheduled) getExecution() *Execution {
	return e.Execution
}

type EventQueued struct {
	At        time.Time
	Execution *Execution
}

func (e EventQueued) getExecution() *Execution {
	return e.Execution
}

type EventStarted struct {
	At        time.Time
	Execution *Execution
}

func (e EventStarted) getExecution() *Execution {
	return e.Execution
}

type EventSkipped struct {
	At        time.Time
	Execution *Execution
	Error     error
}

func (e EventSkipped) getExecution() *Execution {
	return e.Execution
}

type EventReady struct {
	At        time.Time
	Execution *Execution
}

func (e EventReady) getExecution() *Execution {
	return e.Execution
}

type EventSuspended struct {
	At        time.Time
	Execution *Execution
	Bag       *SuspendBag
}

func (e EventSuspended) getExecution() *Execution {
	return e.Execution
}
