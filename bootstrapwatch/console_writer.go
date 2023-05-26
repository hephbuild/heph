package bootstrapwatch

import (
	"errors"
	"os"
	"sync"
)

var ErrClosed = errors.New("closed")
var ErrDrained = errors.New("drained")

type ConsoleWriter struct {
	*os.File
	ch      chan struct{}
	closeCh chan struct{}
	drainCh chan struct{}
	m       sync.Mutex
}

func NewConsoleWriter(w *os.File) *ConsoleWriter {
	cw := &ConsoleWriter{
		File:    w,
		ch:      make(chan struct{}),
		closeCh: make(chan struct{}),
		drainCh: make(chan struct{}),
	}

	return cw
}

func (c *ConsoleWriter) Write(b []byte) (n int, err error) {
	select {
	case <-c.closeCh:
		return 0, ErrClosed
	case <-c.drainCh:
		return 0, ErrDrained
	case <-c.ch:
		return c.File.Write(b)
	}
}

func (c *ConsoleWriter) Close() error {
	close(c.closeCh)

	err := c.File.Close()
	if err != nil {
		return err
	}

	return nil
}

func (c *ConsoleWriter) isConnected() bool {
	select {
	case <-c.ch:
		return true
	default:
		return false
	}
}

func (c *ConsoleWriter) Connect() {
	c.m.Lock()
	defer c.m.Unlock()

	if !c.isConnected() {
		close(c.ch)
	}
}

func (c *ConsoleWriter) Disconnect() {
	c.m.Lock()
	defer c.m.Unlock()

	if c.isConnected() {
		c.ch = make(chan struct{})
	}
}

func (c *ConsoleWriter) Drain() {
	c.m.Lock()
	defer c.m.Unlock()

	close(c.drainCh)
	c.drainCh = make(chan struct{})
}
