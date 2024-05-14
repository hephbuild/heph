package worker2

import "time"

type Event interface {
	Replayable() bool
}

type EventWithExecution interface {
	Event
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
func (e EventCompleted) Replayable() bool {
	return true
}

type EventScheduled struct {
	At        time.Time
	Execution *Execution
}

func (e EventScheduled) getExecution() *Execution {
	return e.Execution
}
func (e EventScheduled) Replayable() bool {
	return true
}

type EventQueued struct {
	At        time.Time
	Execution *Execution
}

func (e EventQueued) getExecution() *Execution {
	return e.Execution
}
func (e EventQueued) Replayable() bool {
	return false
}

type EventStarted struct {
	At        time.Time
	Execution *Execution
}

func (e EventStarted) getExecution() *Execution {
	return e.Execution
}
func (e EventStarted) Replayable() bool {
	return true
}

type EventSkipped struct {
	At        time.Time
	Execution *Execution
	Error     error
}

func (e EventSkipped) getExecution() *Execution {
	return e.Execution
}
func (e EventSkipped) Replayable() bool {
	return true
}

type EventReady struct {
	At        time.Time
	Execution *Execution
}

func (e EventReady) getExecution() *Execution {
	return e.Execution
}
func (e EventReady) Replayable() bool {
	return true
}

type EventSuspended struct {
	At        time.Time
	Execution *Execution
	Bag       *SuspendBag
}

func (e EventSuspended) getExecution() *Execution {
	return e.Execution
}
func (e EventSuspended) Replayable() bool {
	return false
}

type EventNewDep struct {
	Execution *Execution
	Target    Dep
	AddedDep  Dep
}

func (e EventNewDep) getExecution() *Execution {
	return e.Execution
}
func (e EventNewDep) Replayable() bool {
	return false
}
