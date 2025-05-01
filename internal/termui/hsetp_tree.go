package termui

import (
	"iter"
	"maps"
	"slices"
	"strings"
	"time"

	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/lipgloss/tree"
	"github.com/hephbuild/heph/internal/hcore/hstep/hstepfmt"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
)

func buildStepsTreeInner(renderer *lipgloss.Renderer, children map[string][]*corev1.Step, root string, id string) *tree.Tree {
	t := tree.Root(root)

	for _, step := range children[id] {
		t = t.Child(buildStepsTreeInner(renderer, children, hstepfmt.Format(renderer, step, true), step.GetId()))
	}

	return t
}

func buildStepsTree(renderer *lipgloss.Renderer, steps iter.Seq[*corev1.Step]) string {
	children := map[string][]*corev1.Step{}
	for v := range steps {
		if v.GetParentId() == "" && len(children[""]) > 100 {
			continue
		}

		children[v.GetParentId()] = append(children[v.GetParentId()], v)
	}

	for v := range maps.Values(children) {
		slices.SortFunc(v, func(a, b *corev1.Step) int {
			if v := a.GetStartedAt().AsTime().Truncate(1000 * time.Millisecond).Compare(b.GetStartedAt().AsTime().Truncate(1000 * time.Millisecond)); v != 0 {
				return 0
			}

			if v := strings.Compare(a.Id, b.Id); v != 0 {
				return v
			}

			return strings.Compare(a.Text, b.Text)
		})
	}

	t := buildStepsTreeInner(renderer, children, "", "")
	s := renderStepsTree(renderer, t)

	return s
}

func renderStepsTree(renderer *lipgloss.Renderer, t *tree.Tree) string {
	var rootNode tree.Node

	t.
		Enumerator(func(children tree.Children, index int) string {
			if rootNode == nil {
				rootNode = children.At(index)
			}

			if rootNode == children.At(index) {
				if children.Length() == 1 && index == 0 {
					return "◯─"
				} else {
					return "╭─"
				}
			}
			if children.Length()-1 == index {
				return "╰─"
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
	if len(s) > 0 {
		s += "\n"
	}

	return s
}
