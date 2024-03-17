package worker2

import "errors"

type WorkerState int

const (
	WorkerStateUnknown WorkerState = iota
	WorkerStateIdle
	WorkerStateRunning
)

var ErrWorkerNotAvail = errors.New("worker not available")
var ErrNoWorkerAvail = errors.New("no worker available")

type Worker interface {
	Start(a *Execution) error
	State() WorkerState
}

type WorkerProvider interface {
	Start(*Execution) (Worker, error)
	Workers() []Worker
}
