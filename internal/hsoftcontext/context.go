package hsoftcontext

import (
	"context"
	"fmt"
	"sync"
)

type ContextErr struct {
	context.Context
	cancelInner context.CancelFunc

	err error
	m   sync.Mutex
}

func (c *ContextErr) Err() error {
	c.m.Lock()
	err := c.err
	if err == nil {
		err = c.Context.Err()
		if err != nil {
			c.err = err
		}
	}
	c.m.Unlock()

	return err
}

func (c *ContextErr) seterr(err error) {
	c.m.Lock()
	defer c.m.Unlock()

	if c.err != nil {
		return
	}

	c.err = err
}

func (c *ContextErr) cancel(err error) {
	c.m.Lock()
	defer c.m.Unlock()

	if c.err != nil {
		return
	}

	c.cancelInner()
	if err == nil {
		c.err = c.Context.Err()
	} else {
		c.err = fmt.Errorf("%w: %w", c.Context.Err(), err)
	}
}

var _ context.Context = (*ContextErr)(nil)

func WithCancel(octx context.Context) (context.Context, func(err error)) {
	ctx, cancel := context.WithCancel(octx)
	ctx2 := &ContextErr{
		Context:     ctx,
		cancelInner: cancel,
	}
	go func() {
		<-octx.Done()
		ctx2.seterr(octx.Err())
	}()

	return ctx2, ctx2.cancel
}
