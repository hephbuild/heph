package worker

import (
	"context"
	"fmt"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"go.uber.org/multierr"
	"sync/atomic"
	"testing"
	"time"
)

type tracker struct {
	c int32
}

func job(t *testing.T, tr *tracker, id string, d time.Duration) *Job {
	t.Helper()

	return &Job{
		Name: id,
		Do: func(w *Worker, ctx context.Context) error {
			//t.Log(id)
			//defer t.Log(id, "done")

			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-time.After(d):
				if tr != nil {
					atomic.AddInt32(&tr.c, 1)
				}
				return nil
			}
		},
	}
}

func TestSanity(t *testing.T) {
	t.Parallel()

	p := NewPool(1)
	defer p.Stop(nil)
	ctx := context.Background()

	tr := &tracker{}

	j := p.Schedule(ctx, job(t, tr, "j1", time.Second))

	deps := &WaitGroup{}
	deps.Add(j)

	<-deps.Done()
	require.NoError(t, deps.Err())

	assert.Equal(t, int32(1), tr.c)
}

func TestErrorFailFast(t *testing.T) {
	t.Parallel()

	p := NewPool(4)
	defer p.Stop(nil)
	ctx := context.Background()

	tr := &tracker{}
	deps := &WaitGroup{
		failfast: true,
	}

	jerr := p.Schedule(ctx, &Job{
		Name: "jerr",
		Do: func(w *Worker, ctx context.Context) error {
			return fmt.Errorf("failing")
		},
	})
	deps.Add(jerr)

	for i := 0; i < 3; i++ {
		j := p.Schedule(ctx, job(t, tr, fmt.Sprintf("j%v", i), time.Second))
		deps.Add(j)
	}

	<-deps.Done()
	assert.ErrorContains(t, deps.Err(), "failing")

	assert.Equal(t, int32(0), tr.c)
}

func TestErrorFailSlow(t *testing.T) {
	t.Parallel()

	p := NewPool(4)
	defer p.Stop(nil)
	ctx := context.Background()

	tr := &tracker{}
	deps := &WaitGroup{}

	jerr := p.Schedule(ctx, &Job{
		Name: "jerr",
		Do: func(w *Worker, ctx context.Context) error {
			return fmt.Errorf("failing")
		},
	})
	deps.Add(jerr)

	for i := 0; i < 3; i++ {
		j := p.Schedule(ctx, job(t, tr, fmt.Sprintf("j%v", i), time.Second))
		deps.Add(j)
	}

	<-deps.Done()
	assert.ErrorContains(t, deps.Err(), "failing")

	assert.Equal(t, int32(3), tr.c)
}

func TestErrorFailSlowMulti(t *testing.T) {
	t.Parallel()

	p := NewPool(4)
	defer p.Stop(nil)
	ctx := context.Background()

	deps := &WaitGroup{}

	for i := 0; i < 3; i++ {
		jerr := p.Schedule(ctx, &Job{
			Name: fmt.Sprintf("j%v", i),
			Do: func(w *Worker, ctx context.Context) error {
				return fmt.Errorf("failing%v", i)
			},
		})
		deps.Add(jerr)
	}

	<-deps.Done()
	t.Log(deps.Err())
	multierr.Errors(deps.Err())
}

func TestStress(t *testing.T) {
	t.Parallel()

	p := NewPool(1000)
	defer p.Stop(nil)
	ctx := context.Background()

	tr := &tracker{}

	g1 := &WaitGroup{}
	for i := 0; i < 200; i++ {
		j := p.Schedule(ctx, job(t, tr, fmt.Sprintf("g1f%v", i), time.Second))
		g1.Add(j)
	}

	g2 := &WaitGroup{}
	for i := 0; i < 200; i++ {
		j := job(t, tr, fmt.Sprintf("g2f%v", i), time.Second)
		j.Deps = g1
		j = p.Schedule(ctx, j)
		g2.Add(j)
	}

	<-g2.Done()
	require.NoError(t, g2.Err())

	assert.Equal(t, int32(400), tr.c)
}

func TestPause(t *testing.T) {
	t.Parallel()

	// Pool only has a single worker
	p := NewPool(1)
	defer p.Stop(nil)
	ctx := context.Background()

	var c int64

	mj := p.Schedule(ctx, &Job{
		Do: func(w *Worker, ctx context.Context) error {
			// Spawn another job
			j := p.Schedule(ctx, &Job{
				Do: func(w *Worker, ctx context.Context) error {
					atomic.AddInt64(&c, 1)
					return nil
				},
			})

			// Wait for that job, this would deadlock without the Wait
			Wait(ctx, func() {
				<-j.Wait()
			})

			// Repeat

			j = p.Schedule(ctx, &Job{
				Do: func(w *Worker, ctx context.Context) error {
					atomic.AddInt64(&c, 1)
					return nil
				},
			})

			Wait(ctx, func() {
				<-j.Wait()
			})

			return nil
		},
	})

	<-mj.Wait()

	assert.Equal(t, int64(2), c)
}
