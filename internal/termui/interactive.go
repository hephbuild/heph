package termui

import (
	"context"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/lipgloss/tree"
	"github.com/hephbuild/hephv2/internal/hbbt/hbbtexec"
	"github.com/hephbuild/hephv2/internal/hbbt/hbbtlog"
	"github.com/hephbuild/hephv2/internal/hcore/hlog"
	"github.com/hephbuild/hephv2/internal/hcore/hstep"
	"github.com/hephbuild/hephv2/internal/hcore/hstep/hstepfmt"
	"github.com/hephbuild/hephv2/internal/hlipgloss"
	"github.com/hephbuild/hephv2/internal/hpanic"
	corev1 "github.com/hephbuild/hephv2/plugin/gen/heph/core/v1"
	"maps"
	"os"
	"slices"
	"strings"
	"time"
)

type Model struct {
	log      hbbtlog.Hijacker
	Exec     hbbtexec.Model
	renderer *lipgloss.Renderer

	width  int
	height int

	stepCh chan *corev1.Step
	steps  map[string]*corev1.Step
}

func initialModel() Model {
	steps := map[string]*corev1.Step{}
	m := Model{
		log:      hbbtlog.NewLogHijacker(),
		stepCh:   make(chan *corev1.Step),
		steps:    steps,
		renderer: hlipgloss.NewRenderer(os.Stderr),
	}
	m.Exec = hbbtexec.New(m.log)

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

func (m Model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	var cmds []tea.Cmd

	switch msg := msg.(type) {
	case *corev1.Step:
		m.steps[msg.Id] = msg

		if msg.Status == corev1.Step_STATUS_COMPLETED {
			if msg.ParentId == "" {
				cmds = append(cmds, tea.Println(hstepfmt.Format(m.renderer, msg, false)))
			}

			delete(m.steps, msg.Id)
		}

		cmds = append(cmds, m.nextStep())
	case tickMsg:
		cmds = append(cmds, m.nextTick())
	case tea.WindowSizeMsg:
		m.width = msg.Width
		m.height = msg.Height
	}

	cmds, m.log = ChildUpdate(cmds, msg, m.log)

	return m, tea.Batch(cmds...)
}

func (m Model) printTree(children map[string][]*corev1.Step, root string, id string) *tree.Tree {
	t := tree.Root(root)

	for _, step := range children[id] {
		t = t.Child(m.printTree(children, hstepfmt.Format(m.renderer, step, true), step.Id))
	}

	return t
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

	t := m.printTree(children, "", "").
		Enumerator(func(children tree.Children, index int) string {
			if children.Length()-1 == index {
				return "╰─"
			}
			if index == 0 {
				return "╭─"
			}
			return "├─"
		}).
		Indenter(func(children tree.Children, index int) string {
			if children.Length()-1 == index {
				return "  "
			}
			return "│ "
		})

	s := t.String()
	if m.height > 0 {
		s = lipgloss.NewStyle().MaxHeight(m.height).Render(s)
	}
	if len(s) > 0 {
		s += "\n"
	}

	return s
}

func (m Model) View() string {
	var sb strings.Builder

	sb.WriteString(m.buildStepsTree())

	return sb.String()
}

func NewInteractive(ctx context.Context, f func(ctx context.Context, m Model, send func(tea.Msg)) error) error {
	errCh := make(chan error, 1)
	m := initialModel()

	p := tea.NewProgram(m, tea.WithOutput(os.Stderr))
	go func() {
		ctx := ctx
		ctx = hlog.NewContextWithHijacker(ctx, m.log.Handler)

		ctx = hstep.ContextWithHandler(ctx, func(ctx context.Context, step *corev1.Step) *corev1.Step {
			if m.log.GetModeWait() == hbbtlog.LogHijackerModeHijack {
				p.Send(step)
			} else {
				hlog.From(ctx).Info(hstepfmt.Format(m.renderer, step, false))
			}

			return step
		})

		err := hpanic.Recover(func() error {
			return f(ctx, m, p.Send)
		})

		errCh <- err
		p.Send(tea.Quit())
	}()

	_, err := p.Run()
	if err != nil {
		return err
	}

	return <-errCh
}
