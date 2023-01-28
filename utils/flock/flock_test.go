package flock

import (
	"github.com/stretchr/testify/require"
	"os"
	"path/filepath"
	"sync"
	"testing"
	"time"
)

func TestFlock(t *testing.T) {
	dir, err := os.MkdirTemp("", "flock")
	require.NoError(t, err)
	defer os.RemoveAll(dir)

	ch := make(chan struct{}, 1)

	var wg sync.WaitGroup

	do := func() {
		defer wg.Done()

		l := NewFlock("lock", filepath.Join(dir, "lock.lock"))

		err := l.Lock()
		require.NoError(t, err)

		select {
		case ch <- struct{}{}:
			// ok
			time.Sleep(100 * time.Millisecond)
			<-ch
		default:
			panic("lock didnt work...")
		}

		err = l.Unlock()
		require.NoError(t, err)
	}

	for i := 0; i < 5; i++ {
		wg.Add(1)
		go do()
	}

	wg.Wait()
}
