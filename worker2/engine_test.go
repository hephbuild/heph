package worker2

import (
	"context"
	"fmt"
	"github.com/stretchr/testify/assert"
	"testing"
)

func TestExecSimple(t *testing.T) {
	t.Parallel()
	didRun := false
	a := &Action{
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			didRun = true
			fmt.Println("Running 1")
			return nil
		},
	}

	e := NewEngine()

	go e.Run(context.Background())

	e.Schedule(a)

	e.Wait()

	assert.True(t, didRun)
}

func TestExecHook(t *testing.T) {
	t.Parallel()
	ch := make(chan Event, 1000)
	a := &Action{
		Hooks: []Hook{
			func(event Event) {
				ch <- event
			},
		},
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			fmt.Println("Running 1")
			os.Set(NewValue(1))
			return nil
		},
	}

	outputCh := a.OutputCh()

	e := NewEngine()

	go e.Run(context.Background())

	e.Schedule(a)

	e.Wait()
	close(ch)

	events := make([]string, 0)
	for event := range ch {
		events = append(events, fmt.Sprintf("%T", event))
	}

	assert.EqualValues(t, []string{"worker2.EventScheduled", "worker2.EventReady", "worker2.EventStarted", "worker2.EventCompleted"}, events)
	v, _ := (<-outputCh).Get()
	assert.Equal(t, int(1), v)
}

func TestExecError(t *testing.T) {
	t.Parallel()
	a := &Action{
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			return fmt.Errorf("beep bop")
		},
	}

	errCh := a.ErrorCh()

	e := NewEngine()

	go e.Run(context.Background())

	e.Schedule(a)

	e.Wait()

	assert.ErrorContains(t, <-errCh, "beep bop")
}

func TestExecErrorSkip(t *testing.T) {
	t.Parallel()
	a1 := &Action{
		ID: "a1",
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			return fmt.Errorf("beep bop")
		},
	}

	a2 := &Action{
		ID:   "a2",
		Deps: []Dep{a1},
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			return nil
		},
	}

	a3 := &Action{
		ID:   "a3",
		Deps: []Dep{a2},
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			return nil
		},
	}

	err1Ch := a1.ErrorCh()
	err2Ch := a2.ErrorCh()
	err3Ch := a3.ErrorCh()

	e := NewEngine()
	e.RegisterHook(LogHook())

	go e.Run(context.Background())

	e.Schedule(a3)

	e.Wait()

	assert.ErrorContains(t, <-err1Ch, "beep bop")
	assert.ErrorIs(t, <-err2Ch, ErrSkipped)
	assert.ErrorIs(t, <-err3Ch, ErrSkipped)
}

func TestExecErrorSkipStress(t *testing.T) {
	t.Parallel()
	a1 := &Action{
		ID: "a1",
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			return fmt.Errorf("beep bop")
		},
	}

	g := &Group{}

	var errChs []<-chan error

	for i := 0; i < 100; i++ {
		a2 := &Action{
			ID:   fmt.Sprintf("2-%v", i),
			Deps: []Dep{a1},
			Do: func(ctx context.Context, ds InStore, os OutStore) error {
				return nil
			},
		}

		errChs = append(errChs, a2.ErrorCh())

		for j := 0; j < 100; j++ {
			a3 := &Action{
				ID:   fmt.Sprintf("3-%v", j),
				Deps: []Dep{a2},
				Do: func(ctx context.Context, ds InStore, os OutStore) error {
					return nil
				},
			}
			g.Add(a3)

			errChs = append(errChs, a3.ErrorCh())
		}
	}

	e := NewEngine()

	go e.Run(context.Background())

	e.Schedule(g)

	e.Wait()

	for _, errCh := range errChs {
		assert.ErrorIs(t, <-errCh, ErrSkipped)
	}
}

func TestExecCancel(t *testing.T) {
	t.Parallel()
	a := &Action{
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			<-ctx.Done()
			return ctx.Err()
		},
	}

	errCh := a.ErrorCh()

	e := NewEngine()

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	go e.Run(ctx)

	e.Schedule(a)

	cancel()

	e.Wait()

	err := <-errCh

	assert.ErrorIs(t, err, context.Canceled)
}

func TestExecDeps(t *testing.T) {
	t.Parallel()
	a1_1 := &Action{
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			fmt.Println("Running 1")

			os.Set(NewValue(1))
			return nil
		},
	}
	a1_2 := &Action{
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			fmt.Println("Running 2")

			os.Set(NewValue("hello, world"))
			return nil
		},
	}

	receivedValue := ""

	a2 := &Action{
		Deps: []Dep{
			Named{Name: "v1", Dep: a1_1},
			Named{Name: "v2", Dep: a1_2},
		},
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			v1 := ds.Get("v1")
			v2 := ds.Get("v2")

			fmt.Println("Got values", v1, v2)

			receivedValue = fmt.Sprintf("%v %v", v1, v2)
			return nil
		},
	}

	e := NewEngine()

	go e.Run(context.Background())

	e.Schedule(a2)

	e.Wait()

	assert.Equal(t, "1 hello, world", receivedValue)
}

func TestExecGroup(t *testing.T) {
	t.Parallel()
	a1_1 := &Action{
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			fmt.Println("Running 1")

			os.Set(NewValue(1))
			return nil
		},
	}
	a1_2 := &Action{
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			fmt.Println("Running 2")

			os.Set(NewValue("hello, world"))
			return nil
		},
	}

	g := &Group{
		Deps: []Dep{
			Named{Name: "v1", Dep: a1_1},
			Named{Name: "v2", Dep: a1_2},
		},
	}

	var received any
	a := &Action{
		Deps: []Dep{Named{Name: "v", Dep: g}},
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			received = ds.Get("v")
			return nil
		},
	}

	e := NewEngine()

	go e.Run(context.Background())

	e.Schedule(a)

	e.Wait()

	assert.Equal(t, map[string]any{"v1": 1, "v2": "hello, world"}, received)
}

func TestExecStress(t *testing.T) {
	t.Parallel()
	g := &Group{
		Deps: []Dep{},
	}

	n := 30000

	for i := 0; i < n; i++ {
		i := i
		a := &Action{
			Do: func(ctx context.Context, ds InStore, os OutStore) error {
				os.Set(NewValue(i))
				return nil
			},
		}

		g.Add(Named{Name: fmt.Sprint(i), Dep: a})
	}

	var received any
	a := &Action{
		Deps: []Dep{Named{Name: "v", Dep: g}},
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			received = ds.Get("v")
			return nil
		},
	}

	e := NewEngine()

	go e.Run(context.Background())

	e.Schedule(a)

	e.Wait()

	expected := map[string]any{}
	for i := 0; i < n; i++ {
		expected[fmt.Sprint(i)] = i
	}

	assert.Equal(t, expected, received)
}

func TestExecProducerConsumer(t *testing.T) {
	t.Parallel()
	g := &Group{
		Deps: []Dep{},
	}

	n := 10000

	producer := &Action{
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			fmt.Println("Running producer")

			for i := 0; i < n; i++ {
				i := i

				a := &Action{
					Do: func(ctx context.Context, ds InStore, os OutStore) error {
						fmt.Println("Running inner", i)
						os.Set(NewValue(i))
						return nil
					},
				}

				g.Add(Named{Name: fmt.Sprint(i), Dep: a})
			}
			return nil
		},
	}

	g.Add(producer)

	var received any
	consumer := &Action{
		Deps: []Dep{producer, Named{Name: "v", Dep: g}},
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			fmt.Println("Running consumer")

			received = ds.Get("v")
			return nil
		},
	}

	e := NewEngine()

	go e.Run(context.Background())

	e.Schedule(consumer)

	e.Wait()

	expected := map[string]any{}
	for i := 0; i < n; i++ {
		expected[fmt.Sprint(i)] = i
	}

	assert.Equal(t, expected, received)
}

func TestSuspend(t *testing.T) {
	t.Parallel()
	logCh := make(chan string)
	log := func(s string) {
		fmt.Println(s)
		logCh <- s
	}
	resumeCh := make(chan struct{})
	resumeAckCh := make(chan struct{})
	eventCh := make(chan Event, 1000)
	a := &Action{
		Hooks: []Hook{
			func(event Event) {
				eventCh <- event
			},
		},
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			log("enter")
			Wait(ctx, func() {
				log("start_wait")
				<-resumeCh
				resumeAckCh <- struct{}{}
				log("end_wait")
			})
			log("leave")
			return nil
		},
	}

	e := NewEngine()

	go e.Run(context.Background())

	e.Schedule(a)

	assert.Equal(t, "enter", <-logCh)
	assert.Equal(t, "start_wait", <-logCh)
	close(resumeCh)
	<-resumeAckCh
	assert.Equal(t, "end_wait", <-logCh)
	assert.Equal(t, "leave", <-logCh)

	e.Wait()
	close(eventCh)

	events := make([]string, 0)
	for event := range eventCh {
		events = append(events, fmt.Sprintf("%T", event))
	}
	assert.EqualValues(t, []string{"worker2.EventScheduled", "worker2.EventReady", "worker2.EventStarted", "worker2.EventSuspended", "worker2.EventReady", "worker2.EventStarted", "worker2.EventCompleted"}, events)
}
