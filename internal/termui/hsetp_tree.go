package termui

import (
	"fmt"
	"strings"
	"time"

	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/internal/hcore/hstep/hstepfmt"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
)

func buildStepsTree(renderer *lipgloss.Renderer, steps map[string]*corev1.Step, leafs []*corev1.Step, sb *strings.Builder, width, height int) {
	var i int
	for _, leaf := range leafs {
		if time.Since(leaf.GetStartedAt().AsTime()) < 100*time.Millisecond {
			continue
		}

		i++
		sb.WriteString(hstepfmt.FormatStatus(renderer, leaf))
		if d := hstepfmt.FormatDuration(leaf); d != "" {
			_, _ = fmt.Fprintf(sb, " %-5s", d)
		}
		sb.WriteString(" ")
		renderStepsNested(renderer, steps, leaf, sb)
		sb.WriteString("\n")

		if i >= height-1 {
			_, _ = fmt.Fprintf(sb, "%v more", len(leafs)-i)

			break
		}
	}
}

func renderStepsNested(renderer *lipgloss.Renderer, index map[string]*corev1.Step, step *corev1.Step, sb *strings.Builder) {
	if step.GetParentId() != "" {
		step, ok := index[step.GetParentId()]
		if ok {
			renderStepsNested(renderer, index, step, sb)
			sb.WriteString(" > ")
		}
	}

	sb.WriteString(step.GetText())
}
