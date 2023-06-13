package xsync

import (
	"github.com/stretchr/testify/assert"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func TestCondWaiter(t *testing.T) {
	var m sync.Mutex
	c := NewWait(&m)

	doneCh := make(chan struct{})
	var wg sync.WaitGroup

	done := false

	var doneCount int64

	for i := 0; i < 10; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()

			<-c.Wait(func() bool {
				return done
			})
			atomic.AddInt64(&doneCount, 1)
		}()
	}

	go func() {
		wg.Wait()
		<-time.After(5 * time.Second)
		close(doneCh)
	}()

	go func() {
		<-time.After(time.Second)
		done = true
		c.Broadcast()
	}()

	select {
	case <-c.Wait(func() bool {
		return done
	}):
		wg.Wait()
	case <-doneCh:
	}

	assert.Equal(t, int64(10), doneCount)
	assert.Equal(t, true, done)
}
