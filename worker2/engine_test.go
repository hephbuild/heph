package worker2

import (
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/status"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"runtime"
	"testing"
	"time"
)

// Number of actions to be processed during a stress test
const StressN = 100000

// TODO: figure out
var ErrSkipped = errors.New("skipped")

func TestExecSimple(t *testing.T) {
	t.Parallel()

	didRun := false
	a := &Action{
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			didRun = true
			fmt.Println("Running  1")
			return nil
		},
	}

	e := NewEngine()

	go e.Run()
	defer e.Stop()

	e.Schedule(a)

	<-e.Wait()
	<-a.Wait()

	assert.True(t, didRun)
}

func TestExecSerial(t *testing.T) {
	t.Parallel()
	n := 500

	values := make([]int, 0, n)
	expected := make([]int, 0, n)
	deps := make([]Dep, 0, n)

	for i := 0; i < n; i++ {
		i := i
		expected = append(expected, i)
		a := &Action{
			Name: fmt.Sprint(i),
			Do: func(ctx context.Context, ins InStore, outs OutStore) error {
				values = append(values, i)
				return nil
			},
		}
		deps = append(deps, a)
	}

	serial := Serial(deps)

	e := NewEngine()

	go e.Run()
	defer e.Stop()

	e.Schedule(serial)

	<-serial.Wait()

	assert.EqualValues(t, expected, values)
}

func TestDependOnImplicitlyScheduledGroupExecSimple(t *testing.T) {
	t.Parallel()

	g1 := &Group{}

	a1 := &Action{
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			fmt.Println("Running  1")
			return nil
		},
	}

	g1.AddDep(a1)

	e := NewEngine()

	go e.Run()
	defer e.Stop()

	e.Schedule(a1)

	<-g1.Wait()
	<-e.Wait()
}

func TestStatus(t *testing.T) {
	t.Parallel()

	emittedCh := make(chan struct{})
	resumeCh := make(chan struct{})
	a := &Action{
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			status.Emit(ctx, status.String("hello"))
			close(emittedCh)
			<-resumeCh
			return nil
		},
	}

	e := NewEngine()

	go e.Run()
	defer e.Stop()

	e.Schedule(a)

	<-emittedCh

	var emittedStatus status.Statuser
	for _, worker := range e.GetWorkers() {
		emittedStatus = worker.status
		if emittedStatus != nil {
			break
		}
	}
	require.NotNil(t, emittedStatus)
	assert.Equal(t, "hello", emittedStatus.String(nil))

	close(resumeCh)

	<-a.Wait()
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

	go e.Run()
	defer e.Stop()

	e.Schedule(a)

	<-e.Wait()
	close(ch)

	events := make([]string, 0)
	for event := range ch {
		events = append(events, fmt.Sprintf("%T", event))
	}

	assert.EqualValues(t, []string{"worker2.EventScheduled", "worker2.EventQueued", "worker2.EventReady", "worker2.EventStarted", "worker2.EventCompleted"}, events)
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

	go e.Run()
	defer e.Stop()

	e.Schedule(a)

	<-a.Wait()

	assert.ErrorContains(t, <-errCh, "beep bop")
}

func TestExecErrorSkip(t *testing.T) {
	t.Parallel()
	a1 := &Action{
		Name: "a1",
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			return fmt.Errorf("beep bop")
		},
	}

	a2 := &Action{
		Name: "a2",
		Deps: NewDeps(a1),
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			return nil
		},
	}

	a3 := &Action{
		Name: "a3",
		Deps: NewDeps(a2),
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			return nil
		},
	}

	err1Ch := a1.ErrorCh()
	err2Ch := a2.ErrorCh()
	err3Ch := a3.ErrorCh()

	e := NewEngine()
	e.RegisterHook(LogHook())

	go e.Run()
	defer e.Stop()

	e.Schedule(a3)

	<-a3.Wait()

	assert.ErrorContains(t, <-err1Ch, "beep bop")
	assert.ErrorIs(t, <-err2Ch, ErrSkipped)
	assert.ErrorIs(t, <-err3Ch, ErrSkipped)
}

func TestExecErrorSkipStress(t *testing.T) {
	t.Parallel()
	a1 := &Action{
		Name: "a1",
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			return fmt.Errorf("beep bop")
		},
	}

	g := &Group{}

	scheduler := NewLimitScheduler(runtime.NumCPU())

	var errChs []<-chan error

	for i := 0; i < StressN/100; i++ {
		a2 := &Action{
			Name:      fmt.Sprintf("2-%v", i),
			Deps:      NewDeps(a1),
			Scheduler: scheduler,
			Do: func(ctx context.Context, ds InStore, os OutStore) error {
				return nil
			},
		}

		errChs = append(errChs, a2.ErrorCh())

		for j := 0; j < 100; j++ {
			a3 := &Action{
				Name:      fmt.Sprintf("3-%v", j),
				Deps:      NewDeps(a2),
				Scheduler: scheduler,
				Do: func(ctx context.Context, ds InStore, os OutStore) error {
					return nil
				},
			}
			g.AddDep(a3)

			errChs = append(errChs, a3.ErrorCh())
		}
	}

	e := NewEngine()

	go e.Run()
	defer e.Stop()

	e.Schedule(g)

	<-g.Wait()

	for _, errCh := range errChs {
		assert.ErrorIs(t, <-errCh, ErrSkipped)
	}
}

func TestExecCancel(t *testing.T) {
	t.Parallel()

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	a := &Action{
		Ctx: ctx,
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			<-ctx.Done()
			return ctx.Err()
		},
	}

	errCh := a.ErrorCh()

	e := NewEngine()

	go e.Run()
	defer e.Stop()

	e.Schedule(a)

	cancel()

	<-a.Wait()

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
		Deps: NewDeps(
			Named{Name: "v1", Dep: a1_1},
			Named{Name: "v2", Dep: a1_2},
		),
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			v1 := ds.Get("v1")
			v2 := ds.Get("v2")

			fmt.Println("Got values", v1, v2)

			receivedValue = fmt.Sprintf("%v %v", v1, v2)
			return nil
		},
	}

	e := NewEngine()

	go e.Run()
	defer e.Stop()

	e.Schedule(a2)

	<-a2.Wait()

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
		Deps: NewDeps(
			Named{Name: "v1", Dep: a1_1},
			Named{Name: "v2", Dep: a1_2},
		),
	}

	var received any
	a := &Action{
		Deps: NewDeps(Named{Name: "v", Dep: g}),
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			received = ds.Get("v")
			return nil
		},
	}

	e := NewEngine()

	go e.Run()
	defer e.Stop()

	e.Schedule(a)

	<-a.Wait()

	assert.Equal(t, map[string]any{"v1": 1, "v2": "hello, world"}, received)
}

func TestExecStress(t *testing.T) {
	t.Parallel()
	scheduler := NewLimitScheduler(runtime.NumCPU())

	g := &Group{Deps: NewDeps()}

	n := StressN

	for i := 0; i < n; i++ {
		i := i
		a := &Action{
			Scheduler: scheduler,
			Deps:      NewDeps(),
			Do: func(ctx context.Context, ds InStore, os OutStore) error {
				os.Set(NewValue(i))
				return nil
			},
		}

		g.AddDep(Named{Name: fmt.Sprint(i), Dep: a})
	}

	var received any
	a := &Action{
		Deps: NewDeps(Named{Name: "v", Dep: g}),
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			received = ds.Get("v")
			return nil
		},
	}

	e := NewEngine()

	go e.Run()
	defer e.Stop()

	totalDeps := uint64(n + 2)

	stats1 := CollectStats(a)
	assert.Equal(t, Stats{All: totalDeps}, stats1)

	e.Schedule(a)

	<-a.Wait()

	stats3 := CollectStats(a)
	assert.Equal(t, Stats{All: totalDeps, Completed: totalDeps, Succeeded: totalDeps}, stats3)

	expected := map[string]any{}
	for i := 0; i < n; i++ {
		expected[fmt.Sprint(i)] = i
	}

	assert.Equal(t, expected, received)
}

func TestExecProducerConsumer(t *testing.T) {
	g := &Group{
		Deps: NewDeps(),
	}

	n := 10000

	producer := &Action{
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			fmt.Println("Running producer")

			for i := 0; i < n; i++ {
				i := i

				a := &Action{
					Do: func(ctx context.Context, ds InStore, os OutStore) error {
						//fmt.Println("Running inner", i)
						os.Set(NewValue(i))
						return nil
					},
				}

				g.AddDep(Named{Name: fmt.Sprint(i), Dep: a})
			}
			return nil
		},
	}

	g.AddDep(producer)

	var received any
	consumer := &Action{
		Deps: NewDeps(producer, Named{Name: "v", Dep: g}),
		Do: func(ctx context.Context, ds InStore, os OutStore) error {
			fmt.Println("Running consumer")

			received = ds.Get("v")
			return nil
		},
	}

	e := NewEngine()

	go e.Run()
	defer e.Stop()

	e.Schedule(consumer)

	<-consumer.Wait()

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

	go e.Run()
	defer e.Stop()

	e.Schedule(a)

	assert.Equal(t, "enter", <-logCh)
	assert.Equal(t, "start_wait", <-logCh)
	close(resumeCh)
	<-resumeAckCh
	assert.Equal(t, "end_wait", <-logCh)
	assert.Equal(t, "leave", <-logCh)

	<-e.Wait()
	close(eventCh)

	events := make([]string, 0)
	for event := range eventCh {
		events = append(events, fmt.Sprintf("%T", event))
	}
	assert.EqualValues(t, []string{"worker2.EventScheduled", "worker2.EventQueued", "worker2.EventReady", "worker2.EventStarted", "worker2.EventSuspended", "worker2.EventReady", "worker2.EventStarted", "worker2.EventCompleted"}, events)
}

func TestSuspendLimit(t *testing.T) {
	t.Parallel()

	e := NewEngine()
	e.SetDefaultScheduler(NewLimitScheduler(1))

	g := &Group{}

	for i := 0; i < 100; i++ {
		a := &Action{
			Do: func(ctx context.Context, ins InStore, outs OutStore) error {
				Wait(ctx, func() {
					time.Sleep(time.Second)
				})
				return nil
			},
		}
		g.AddDep(a)

		e.Schedule(a)
	}

	go e.Run()
	defer e.Stop()

	<-g.Wait()
	<-e.Wait()
}
