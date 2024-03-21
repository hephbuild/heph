package worker2

type Event any

type WithExecution interface {
	getExecution() *Execution
}

type EventTryExecuteOne struct{}

type EventCompleted struct {
	Execution *Execution
	Output    Value
	Error     error
}

func (e EventCompleted) getExecution() *Execution {
	return e.Execution
}

type EventScheduled struct {
	Execution *Execution
}

func (e EventScheduled) getExecution() *Execution {
	return e.Execution
}

type EventStarted struct {
	Execution *Execution
}

func (e EventStarted) getExecution() *Execution {
	return e.Execution
}

type EventSkipped struct {
	Execution *Execution
}

func (e EventSkipped) getExecution() *Execution {
	return e.Execution
}

type EventWorkerAvailable struct {
	Worker *Worker
}

type EventReady struct {
	Execution *Execution
}

func (e EventReady) getExecution() *Execution {
	return e.Execution
}

type EventSuspended struct {
	Execution *Execution
}

func (e EventSuspended) getExecution() *Execution {
	return e.Execution
}
