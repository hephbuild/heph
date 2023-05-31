package locks

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/log/liblog"
	"github.com/hephbuild/heph/log/testlog"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func TestFlock(t *testing.T) {
	dir, err := os.MkdirTemp("", "flock")
	require.NoError(t, err)
	defer os.RemoveAll(dir)

	name := t.Name()
	p := filepath.Join(dir, name+".lock")

	testLocker(t, func() Locker {
		return NewFlock(name, p)
	})
}

func TestFlockContext(t *testing.T) {
	dir, err := os.MkdirTemp("", "flock")
	require.NoError(t, err)
	defer os.RemoveAll(dir)

	name := t.Name()
	p := filepath.Join(dir, name+".lock")

	testLockerContext(t, func() Locker {
		return NewFlock(name, p)
	})
}

func TestFlockTry(t *testing.T) {
	dir, err := os.MkdirTemp("", "flock")
	require.NoError(t, err)
	defer os.RemoveAll(dir)

	name := t.Name()
	p := filepath.Join(dir, name+".lock")

	testLockerTry(t, func() Locker {
		return NewFlock(name, p)
	})
}

func TestFlockSingleInstance(t *testing.T) {
	dir, err := os.MkdirTemp("", "flock")
	require.NoError(t, err)
	defer os.RemoveAll(dir)

	l := NewFlock("lock", filepath.Join(dir, t.Name()+".lock"))

	testLocker(t, func() Locker {
		return l
	})
}

func TestFlockSingleInstanceContext(t *testing.T) {
	dir, err := os.MkdirTemp("", "flock")
	require.NoError(t, err)
	defer os.RemoveAll(dir)

	l := NewFlock("lock", filepath.Join(dir, t.Name()+".lock"))

	testLockerContext(t, func() Locker {
		return l
	})
}

func TestFlockSingleInstanceTry(t *testing.T) {
	dir, err := os.MkdirTemp("", "flock")
	require.NoError(t, err)
	defer os.RemoveAll(dir)

	l := NewFlock("lock", filepath.Join(dir, t.Name()+".lock"))

	testLockerTry(t, func() Locker {
		return l
	})
}

func TestMutex(t *testing.T) {
	l := NewMutex("lock")
	testLocker(t, func() Locker {
		return l
	})
}

func TestMutexContext(t *testing.T) {
	l := NewMutex("lock")
	testLockerContext(t, func() Locker {
		return l
	})
}

func TestMutexTry(t *testing.T) {
	l := NewMutex("lock")
	testLockerTry(t, func() Locker {
		return l
	})
}

func testLocker(t *testing.T, factory func() Locker) {
	var wg sync.WaitGroup
	var n int32

	logger := testlog.NewLogger(t)
	ctx := liblog.ContextWith(context.Background(), logger)

	do := func() {
		defer wg.Done()

		l := factory()

		err := l.Lock(ctx)
		if err != nil {
			panic(err)
		}

		if v := atomic.AddInt32(&n, 1); v != 1 {
			panic(fmt.Sprintf("lock didnt work, got concurrent access 1: %v", v))
		}

		time.Sleep(time.Millisecond)

		if v := atomic.AddInt32(&n, -1); v != 0 {
			panic(fmt.Sprintf("lock didnt work, got concurrent access 2: %v", v))
		}

		err = l.Unlock()
		if err != nil {
			panic(err)
		}
	}

	for i := 0; i < 1000; i++ {
		wg.Add(1)
		go do()
	}

	wg.Wait()
}

func testLockerContext(t *testing.T, factory func() Locker) {
	doneCh := make(chan struct{})
	defer close(doneCh)

	ch := make(chan struct{})

	logger := testlog.NewLogger(t)
	ctx := liblog.ContextWith(context.Background(), logger)

	go func() {
		l := factory()

		err := l.Lock(ctx)
		if err != nil {
			panic(err)
		}
		// hold lock
		ch <- struct{}{}
	}()

	<-ch
	t.Log("other routine got lock, locking here")

	ctx, cancel := context.WithTimeout(ctx, time.Second)
	defer cancel()

	go func() {
		<-ctx.Done()
		t.Log("CTX DONE:", ctx.Err())
		select {
		case <-time.After(5 * time.Second):
			panic("should have already finished")
		case <-doneCh:
			// all good
		}
	}()

	l := factory()

	err := l.Lock(ctx)
	assert.ErrorContains(t, err, "context deadline exceeded")

	time.Sleep(time.Second)
}

func testLockerTry(t *testing.T, factory func() Locker) {
	l1 := factory()
	l2 := factory()

	logger := testlog.NewLogger(t)
	ctx := liblog.ContextWith(context.Background(), logger)

	err := l1.Lock(ctx)
	require.NoError(t, err)

	ok, err := l2.TryLock(ctx)
	require.NoError(t, err)
	assert.False(t, ok)

	err = l1.Unlock()
	require.NoError(t, err)

	ok, err = l2.TryLock(ctx)
	require.NoError(t, err)
	assert.True(t, ok)
}
