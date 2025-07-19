package termui

import (
	"maps"
	"slices"
	"strings"
	"time"

	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/lipgloss/tree"
	"github.com/hephbuild/heph/internal/hcore/hstep/hstepfmt"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
)

func buildStepsTreeInner(renderer *lipgloss.Renderer, children map[string][]*corev1.Step, root string, id string, depth int) *tree.Tree {
	if depth > 5 {
		return tree.Root(root + " max depth exceeded")
	}

	t := tree.Root(root)

	for _, step := range children[id] {
		t = t.Child(buildStepsTreeInner(renderer, children, hstepfmt.Format(renderer, step, true), step.GetId(), depth+1))
	}

	return t
}

func buildStepsTree(renderer *lipgloss.Renderer, steps []*corev1.Step) string {
	children := map[string][]*corev1.Step{}
	for _, v := range steps {
		if v.GetParentId() == "" && len(children[""]) > 100 {
			continue
		}

		children[v.GetParentId()] = append(children[v.GetParentId()], v)
	}

	for v := range maps.Values(children) {
		slices.SortFunc(v, func(a, b *corev1.Step) int {
			if v := a.GetStartedAt().AsTime().Truncate(time.Second).Compare(b.GetStartedAt().AsTime().Truncate(time.Second)); v != 0 {
				return v
			}

			if v := strings.Compare(a.GetId(), b.GetId()); v != 0 {
				return v
			}

			return strings.Compare(a.GetText(), b.GetText())
		})
	}

	t := buildStepsTreeInner(renderer, children, "", "", 0)
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
