package xtime

import (
	"context"
	"github.com/stretchr/testify/assert"
	"testing"
	"time"
)

func TestDebounce(t *testing.T) {
	d := NewDebounce(100 * time.Millisecond)

	ch := make(chan int, 10)

	f := func(i int) func(ctx context.Context) {
		return func(ctx context.Context) {
			select {
			case <-ctx.Done():
				// noop:
			case <-time.After(time.Second):
				ch <- i
			}
		}
	}

	d.Do(f(1))
	d.Do(f(2))
	d.Do(f(3))
	time.Sleep(500 * time.Millisecond)
	d.Do(f(4))

	received := make([]int, 0)

	go func() {
		for i := range ch {
			received = append(received, i)
		}
	}()

	time.Sleep(5 * time.Second)

	assert.EqualValues(t, []int{3}, received)
}
