package worker2

import (
	"context"
	"fmt"
	"github.com/stretchr/testify/assert"
	"testing"
	"time"
)

func TestExecSimple(t *testing.T) {
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
	ch := make(chan Event, 1000)
	outputHook, outputCh := OutputHook()
	a := &Action{
		Hooks: []Hook{
			func(event Event) {
				ch <- event
			},
			outputHook,
		},
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			fmt.Println("Running 1")
			os.Set(MemoryValue[int]{V: 1})
			return nil
		},
	}

	e := NewEngine()

	go e.Run(context.Background())

	e.Schedule(a)

	e.Wait()
	close(ch)

	events := make([]string, 0)
	for event := range ch {
		events = append(events, fmt.Sprintf("%T", event))
	}

	assert.EqualValues(t, []string{"worker2.EventSchedule", "worker2.EventReady", "worker2.EventCompleted"}, events)
	v, _ := (<-outputCh).Get()
	assert.Equal(t, int(1), v)
}

func TestExecCancel(t *testing.T) {
	a := &Action{
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			<-ctx.Done()
			return ctx.Err()
		},
	}

	e := NewEngine()

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	go e.Run(ctx)

	e.Schedule(a)

	time.Sleep(time.Second)
	cancel()

	e.Wait()
}

func TestExecDeps(t *testing.T) {
	a1_1 := &Action{
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			fmt.Println("Running 1")

			os.Set(MemoryValue[int]{V: 1})
			return nil
		},
	}
	a1_2 := &Action{
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			fmt.Println("Running 2")

			os.Set(MemoryValue[string]{V: "hello, world"})
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
			v1, _ := ds.Get("v1")
			v2, _ := ds.Get("v2")

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
	a1_1 := &Action{
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			fmt.Println("Running 1")

			os.Set(MemoryValue[int]{V: 1})
			return nil
		},
	}
	a1_2 := &Action{
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			fmt.Println("Running 2")

			os.Set(MemoryValue[string]{V: "hello, world"})
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
			received, _ = ds.Get("v")
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
	g := &Group{
		Deps: []Dep{},
	}

	n := 1000

	for i := 0; i < n; i++ {
		i := i
		a := &Action{
			Do: func(ctx context.Context, ds InStore, os OutStore) error {
				os.Set(MemoryValue[int]{V: i})
				return nil
			},
		}

		g.Add(Named{Name: fmt.Sprintf("%v", i), Dep: a})
	}

	var received any
	a := &Action{
		Deps: []Dep{Named{Name: "v", Dep: g}},
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			received, _ = ds.Get("v")
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
