package worker

import (
	"context"
	"fmt"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
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
		ID: id,
		Do: func(w *Worker, ctx context.Context) error {
			//t.Log(id)

			time.Sleep(d)
			if tr != nil {
				atomic.AddInt32(&tr.c, 1)
			}

			//t.Log(id, "done")
			return nil
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
