package termui

import (
	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/lipgloss/tree"
	"github.com/hephbuild/hephv2/internal/hcore/hstep/hstepfmt"
	corev1 "github.com/hephbuild/hephv2/plugin/gen/heph/core/v1"
	"iter"
	"maps"
	"slices"
)

func buildStepsTreeInner(renderer *lipgloss.Renderer, children map[string][]*corev1.Step, root string, id string) *tree.Tree {
	t := tree.Root(root)

	for _, step := range children[id] {
		t = t.Child(buildStepsTreeInner(renderer, children, hstepfmt.Format(renderer, step, true), step.Id))
	}

	return t
}

func buildStepsTree(renderer *lipgloss.Renderer, steps iter.Seq[*corev1.Step]) string {
	children := map[string][]*corev1.Step{}
	for v := range steps {
		children[v.ParentId] = append(children[v.ParentId], v)
	}

	for v := range maps.Values(children) {
		slices.SortFunc(v, func(a, b *corev1.Step) int {
			return a.StartedAt.AsTime().Compare(b.StartedAt.AsTime())
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
					return "▢ "
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
