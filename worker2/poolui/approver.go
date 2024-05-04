package poolui

import (
	"context"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/worker2"
	"sync"
)

func NewApprover() *Approver {
	return &Approver{
		queue: make(chan ApproveRequest),
	}
}

type ApproveRequest struct {
	Target  specs.Target
	Respond func(bool)
}

type Approver struct {
	m         sync.Mutex
	connected bool
	queue     chan ApproveRequest
}

func (a *Approver) SetConnected(connected bool) {
	a.m.Lock()
	defer a.m.Unlock()

	a.connected = connected

	if !connected {
		for {
			select {
			case req := <-a.queue:
				req.Respond(false)
			default:
				return
			}
		}
	}
}

func (a *Approver) Approve(ctx context.Context, t specs.Target) bool {
	a.m.Lock()

	if !a.connected {
		a.m.Unlock()
		return false
	}
	a.m.Unlock()

	valueCh := make(chan bool)
	defer close(valueCh)

	err := worker2.WaitChanSend(ctx, a.queue, ApproveRequest{
		Target: t,
		Respond: func(approved bool) {
			valueCh <- approved
		},
	})
	if err != nil {
		return false
	}

	res, err := worker2.WaitChanReceive(ctx, valueCh)
	if err != nil {
		return false
	}

	return res
}

func (a *Approver) Next() ApproveRequest {
	return <-a.queue
}
