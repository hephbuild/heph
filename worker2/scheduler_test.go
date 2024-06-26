package worker2

import (
	"github.com/stretchr/testify/require"
	"golang.org/x/net/context"
	"math/rand"
	"sync"
	"testing"
	"time"
)

func TestResourceScheduler(t *testing.T) {
	s := NewResourceScheduler(map[string]float64{
		"cpu":    1000,
		"memory": 4000,
	}, nil)

	d1 := NewAction(ActionConfig{
		Requests: map[string]float64{
			"cpu": 100,
		},
	})
	err := s.Schedule(d1, nil)
	require.NoError(t, err)

	d2 := NewAction(ActionConfig{
		Requests: map[string]float64{
			"cpu": 100,
		},
	})
	err = s.Schedule(d2, nil)
	require.NoError(t, err)

	ctx, _ := context.WithTimeout(context.Background(), time.Second)
	d3 := NewAction(ActionConfig{
		Ctx: ctx,
		Requests: map[string]float64{
			"cpu": 1000,
		},
	})
	err = s.Schedule(d3, nil)
	require.ErrorIs(t, err, context.DeadlineExceeded)

	s.Done(d1)
	s.Done(d2)

	d4 := NewAction(ActionConfig{
		Requests: map[string]float64{
			"cpu": 1000,
		},
	})
	err = s.Schedule(d4, nil)
	require.NoError(t, err)
}

func TestStressResourceScheduler(t *testing.T) {
	s := NewResourceScheduler(map[string]float64{
		"cpu": 1000,
	}, nil)

	var wg sync.WaitGroup

	for i := 0; i < 10000; i++ {
		wg.Add(1)
		go func() {
			d := NewAction(ActionConfig{
				Requests: map[string]float64{
					"cpu": float64(rand.Intn(200)),
				},
			})
			err := s.Schedule(d, nil)
			require.NoError(t, err)

			go func() {
				time.Sleep(time.Millisecond)
				s.Done(d)
				wg.Done()
			}()
		}()
	}

	wg.Wait()
}
