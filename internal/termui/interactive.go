package termui

import (
	"context"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/hephv2/internal/hbbt/hbbtexec"
	"github.com/hephbuild/hephv2/internal/hbbt/hbbtlog"
	"github.com/hephbuild/hephv2/internal/hcore/hlog"
	"github.com/hephbuild/hephv2/internal/hcore/hstep"
	"github.com/hephbuild/hephv2/internal/hcore/hstep/hstepfmt"
	corev1 "github.com/hephbuild/hephv2/plugin/gen/heph/core/v1"
	"maps"
	"os"
	"slices"
	"strings"
	"time"
)

type Model struct {
	ctx context.Context

	log  hbbtlog.Hijacker
	Exec hbbtexec.Model

	stepCh chan *corev1.Step
	steps  map[string]*corev1.Step
}

func initialModel(ctx context.Context) Model {
	steps := map[string]*corev1.Step{}
	m := Model{
		log:    hbbtlog.NewLogHijacker(),
		stepCh: make(chan *corev1.Step, 100),
		steps:  steps,
	}

	ctx = hlog.NewContextWithHijacker(ctx, m.log.Handler)

	ctx = hstep.ContextWithHandler(ctx, func(ctx context.Context, step *corev1.Step) *corev1.Step {
		if m.log.GetModeWait() == hbbtlog.LogHijackerModeHijack {
			select {
			case m.stepCh <- step:
			default:
				hlog.From(ctx).Info(hstepfmt.Format(step, false))
			}
		} else {
			hlog.From(ctx).Info(hstepfmt.Format(step, false))
		}

		return step
	})

	ctx, m.Exec = hbbtexec.New(ctx, m.log)

	m.ctx = ctx

	return m
}

func (m Model) nextStep() tea.Cmd {
	return func() tea.Msg {
		return <-m.stepCh
	}
}

type tickMsg struct{}

func (m Model) nextTick() tea.Cmd {
	return tea.Every(20*time.Millisecond, func(t time.Time) tea.Msg {
		return tickMsg{}
	})
}

func (m Model) Init() tea.Cmd {
	return tea.Batch(m.log.Init(), m.nextStep(), m.nextTick())
}

func ResetSteps() tea.Cmd {
	return func() tea.Msg {
		return resetStepsMsg{}
	}
}

type resetStepsMsg struct{}

func (m Model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	var cmds []tea.Cmd

	switch msg := msg.(type) {
	case *corev1.Step:
		m.steps[msg.Id] = msg

		cmds = append(cmds, m.nextStep())
	case resetStepsMsg:
		m.steps = map[string]*corev1.Step{}
	case tickMsg:
		cmds = append(cmds, m.nextTick())
	}

	cmds, m.log = ChildUpdate(cmds, msg, m.log)

	return m, tea.Batch(cmds...)
}

func printTree(children map[string][]*corev1.Step, indent, id string) string {
	var sb strings.Builder

	for _, step := range children[id] {
		sb.WriteString(indent)
		sb.WriteString(hstepfmt.Format(step, true))
		sb.WriteString("\n")
		sb.WriteString(printTree(children, "└ "+indent, step.Id))
	}

	return sb.String()
}

func (m Model) buildStepsTree() string {
	children := map[string][]*corev1.Step{}
	for v := range maps.Values(m.steps) {
		children[v.ParentId] = append(children[v.ParentId], v)
	}

	for v := range maps.Values(children) {
		slices.SortFunc(v, func(a, b *corev1.Step) int {
			return a.StartedAt.AsTime().Compare(b.StartedAt.AsTime())
		})
	}

	return printTree(children, "", "")
}

func (m Model) View() string {
	var sb strings.Builder

	sb.WriteString(m.buildStepsTree())

	return sb.String()
}

func NewInteractive(ctx context.Context, f func(ctx context.Context, m Model, send func(tea.Msg)) error) error {
	errCh := make(chan error, 1)
	m := initialModel(ctx)

	p := tea.NewProgram(m, tea.WithOutput(os.Stderr))
	go func() {
		errCh <- f(m.ctx, m, p.Send)
		p.Send(tea.Quit())
	}()

	_, err := p.Run()
	if err != nil {
		return err
	}

	return <-errCh
}
