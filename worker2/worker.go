package worker2

import (
	"errors"
	"github.com/hephbuild/heph/status"
)

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
	status.Handler
}

type WorkerProvider interface {
	Start(*Execution) (Worker, error)
	Workers() []Worker
}
