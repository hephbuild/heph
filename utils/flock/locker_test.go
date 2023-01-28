package flock

import (
	"context"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"os"
	"path/filepath"
	"sync"
	"testing"
	"time"
)

func TestFlock(t *testing.T) {
	t.Parallel()
	dir, err := os.MkdirTemp("", "flock")
	require.NoError(t, err)
	defer os.RemoveAll(dir)

	testLocker(t, func() Locker {
		return NewFlock("lock", filepath.Join(dir, "lock.lock"))
	})
}

func TestFlockContext(t *testing.T) {
	t.Parallel()
	dir, err := os.MkdirTemp("", "flock")
	require.NoError(t, err)
	defer os.RemoveAll(dir)

	testLockerContext(t, func() Locker {
		return NewFlock("lock", filepath.Join(dir, "lock.lock"))
	})
}

func TestFlockSingleInstance(t *testing.T) {
	t.Parallel()
	dir, err := os.MkdirTemp("", "flock")
	require.NoError(t, err)
	defer os.RemoveAll(dir)

	l := NewFlock("lock", filepath.Join(dir, "lock.lock"))

	testLocker(t, func() Locker {
		return l
	})
}

func TestFlockSingleInstanceContext(t *testing.T) {
	t.Parallel()
	dir, err := os.MkdirTemp("", "flock")
	require.NoError(t, err)
	defer os.RemoveAll(dir)

	l := NewFlock("lock", filepath.Join(dir, "lock.lock"))

	testLockerContext(t, func() Locker {
		return l
	})
}

func TestMutex(t *testing.T) {
	t.Parallel()
	l := NewMutex("lock")
	testLocker(t, func() Locker {
		return l
	})
}

func TestMutexContext(t *testing.T) {
	t.Parallel()
	l := NewMutex("lock")
	testLockerContext(t, func() Locker {
		return l
	})
}

func testLocker(t *testing.T, factory func() Locker) {
	ch := make(chan struct{}, 1)

	var wg sync.WaitGroup

	do := func() {
		defer wg.Done()

		l := factory()

		err := l.Lock(context.Background())
		if err != nil {
			panic(err)
		}

		select {
		case ch <- struct{}{}:
			// ok
			time.Sleep(100 * time.Millisecond)
			<-ch
		default:
			panic("lock didnt work, got concurrent access to chan")
		}

		err = l.Unlock()
		if err != nil {
			panic(err)
		}
	}

	for i := 0; i < 5; i++ {
		wg.Add(1)
		go do()
	}

	wg.Wait()
}

func testLockerContext(t *testing.T, factory func() Locker) {
	dir, err := os.MkdirTemp("", "flock")
	require.NoError(t, err)
	defer os.RemoveAll(dir)

	ch := make(chan struct{})

	go func() {
		l := factory()

		err := l.Lock(context.Background())
		if err != nil {
			panic(err)
		}
		// hold lock
		ch <- struct{}{}
	}()

	<-ch
	t.Log("other routine got lock, locking here")

	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()

	go func() {
		<-ctx.Done()
		t.Log(ctx.Err())
		time.Sleep(3 * time.Second)
		panic("should have already exited")
	}()

	l := factory()

	err = l.Lock(ctx)
	assert.ErrorContains(t, err, "acquire lock for lock: context deadline exceeded")
}
