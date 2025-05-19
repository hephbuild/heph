package termui

import (
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/internal/hsoftcontext"
	"maps"
	"os"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/hephbuild/heph/internal/hbbt/hbbtlog"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hcore/hstep/hstepfmt"
	"github.com/hephbuild/heph/internal/hlipgloss"
	"github.com/hephbuild/heph/internal/hpanic"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
)

type stepsUpdateMsg map[string]*corev1.Step

type Model struct {
	log           hbbtlog.Hijacker
	Exec          hbbtexec.Model
	renderer      *lipgloss.Renderer
	cancelRoutine context.CancelCauseFunc

	width  int
	height int

	steps          map[string]*corev1.Step
	pauseRendering bool
}

func initialModel(cancelRoutine context.CancelCauseFunc) Model {
	m := Model{
		log:           hbbtlog.NewLogHijacker(),
		steps:         map[string]*corev1.Step{},
		renderer:      hlipgloss.NewRenderer(os.Stderr),
		cancelRoutine: cancelRoutine,
	}
	m.Exec = hbbtexec.New(m.log)

	return m
}

type tickMsg struct{}

func (m Model) nextTick() tea.Cmd {
	return tea.Every(20*time.Millisecond, func(t time.Time) tea.Msg {
		return tickMsg{}
	})
}

func (m Model) Init() tea.Cmd {
	return tea.Batch(m.log.Init(), m.nextTick())
}

type routineExitedMsg struct{}

func (m Model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	var cmds []tea.Cmd

	switch msg := msg.(type) {
	case stepsUpdateMsg:
		m.steps = msg
	case tickMsg:
		cmds = append(cmds, m.nextTick())
	case tea.WindowSizeMsg:
		m.width = msg.Width
		m.height = msg.Height
	case hbbtexec.StartMsg:
		m.pauseRendering = true
	case hbbtexec.EndMsg:
		m.pauseRendering = false
	case routineExitedMsg:
		cmds = append(cmds, tea.Quit)
	case tea.QuitMsg:
		m.pauseRendering = true
	case tea.InterruptMsg:
		m.cancelRoutine(errors.New("ctrl+c"))
	case tea.KeyMsg:
		switch msg.Type {
		case tea.KeyBreak:
			m.cancelRoutine(errors.New("ctrl+c"))
		}
	}

	cmds, m.log = ChildUpdate(cmds, msg, m.log)

	return m, tea.Batch(cmds...)
}

func (m Model) View() string {
	if m.pauseRendering {
		return ""
	}

	var sb strings.Builder

	sb.WriteString(fmt.Sprintf("%v\n", len(m.steps)))
	stepsTree := buildStepsTree(m.renderer, maps.Values(m.steps))
	if m.height > 0 {
		stepsTree = lipgloss.NewStyle().MaxHeight(m.height / 2).Render(stepsTree)
	}

	sb.WriteString(stepsTree)

	return lipgloss.NewStyle().
		MaxWidth(m.width - 1). // not sure why -1 is needed
		MaxHeight(m.height).
		Render(sb.String())
}

func NewStepsStore(ctx context.Context, p *tea.Program, renderer *lipgloss.Renderer) (func(*corev1.Step), func()) {
	stepsCh := make(chan *corev1.Step)
	steps := map[string]*corev1.Step{}
	var stepsm sync.Mutex

	go func() {
		for step := range stepsCh {
			stepsm.Lock()
			steps[step.GetId()] = step

			if step.GetStatus() == corev1.Step_STATUS_COMPLETED {
				//if step.GetParentId() == "" && step.Error {
				//	hlog.From(ctx).Info(hstepfmt.Format(renderer, step, false))
				//}

				delete(steps, step.GetId())
			}
			stepsm.Unlock()
		}
	}()

	t := time.NewTicker(20 * time.Millisecond)
	go func() {
		for range t.C {
			stepsm.Lock()
			steps := maps.Clone(steps)
			maps.DeleteFunc(steps, func(k string, v *corev1.Step) bool { // prevent stroboscopic effect
				return time.Since(v.StartedAt.AsTime()) < 100*time.Millisecond
			})
			p.Send(stepsUpdateMsg(steps))
			stepsm.Unlock()
		}
	}()

	return func(step *corev1.Step) {
			stepsCh <- step
		}, func() {
			t.Stop()
			close(stepsCh)
		}
}

type RunFunc = func(ctx context.Context, execFunc func(f hbbtexec.ExecFunc) error) error

func NewInteractive(ctx context.Context, f RunFunc) error {
	errCh := make(chan error, 1)

	ctx, cancel := hsoftcontext.WithCancel(ctx)
	defer cancel(nil)

	var currentlyInteractive atomic.Bool

	m := initialModel(func(cause error) {
		if currentlyInteractive.Load() {
			return
		}

		cancel(cause)
	})

	p := tea.NewProgram(m, tea.WithOutput(os.Stderr), tea.WithInput(os.Stdin))
	go func() {
		ctx := ctx
		ctx = hlog.NewContextWithHijacker(ctx, m.log.Handler)

		sendStep, cancelStepStore := NewStepsStore(ctx, p, m.renderer)
		defer cancelStepStore()

		ctx = hstep.ContextWithHandler(ctx, func(ctx context.Context, step *corev1.Step) *corev1.Step {
			if m.log.GetMode() == hbbtlog.LogHijackerModeHijack {
				sendStep(step)
			} else {
				hlog.From(ctx).Info(hstepfmt.Format(m.renderer, step, false))
			}

			return step
		})

		var mu sync.Mutex
		err := hpanic.Recover(func() error {
			return f(ctx, func(f hbbtexec.ExecFunc) error {
				if !mu.TryLock() {
					return fmt.Errorf("two concurrent interractive exec detected")
				}
				defer mu.Unlock()

				currentlyInteractive.Store(true)
				defer currentlyInteractive.Store(false)

				return hbbtexec.Run(m.Exec, p.Send, f)
			})
		})

		errCh <- err
		p.Send(routineExitedMsg{})
	}()

	_, err := p.Run()
	if err != nil {
		return err
	}

	return <-errCh
}
