package worker2

import (
	"errors"
)

type WorkerState int

const (
	WorkerStateUnknown WorkerState = iota
	WorkerStateIdle
	WorkerStateRunning
)

var ErrWorkerNotAvail = errors.New("worker not available")
var ErrNoWorkerAvail = errors.New("no worker available")

type WorkerProvider interface {
	// Start should return ErrNoWorkerAvail if it cannot assign work
	Start(*Execution) (*Worker, error)
	Workers() []*Worker
}
