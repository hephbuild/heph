package worker

import (
	"context"
	"sync"
	"testing"
)

func BenchmarkSchedule(b *testing.B) {
	pool := NewPool(1)
	defer pool.Stop(nil)

	b.ResetTimer()

	var wg sync.WaitGroup
	for i := 0; i < b.N; i++ {
		wg.Add(1)
		pool.Schedule(context.Background(), &Job{
			Do: func(w *Worker, ctx context.Context) error {
				wg.Done()
				return nil
			},
		})
	}

	wg.Wait()
}

func BenchmarkGoroutine(b *testing.B) {
	var wg sync.WaitGroup
	for i := 0; i < b.N; i++ {
		wg.Add(1)
		go func() {
			wg.Done()
		}()
	}
	wg.Wait()
}
