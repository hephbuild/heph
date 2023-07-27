package tgt

import (
	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/status"
)

func TargetStatus(t specs.Specer, status string) status.Statuser {
	return targetStatus{t.Spec().FQN, "", status}
}

type targetStatus struct {
	fqn, output, status string
}

var (
	targetColor = lipgloss.AdaptiveColor{Light: "#FFBB00", Dark: "#FFCA33"}
	targetStyle = struct {
		target, output lipgloss.Style
	}{
		lipgloss.NewStyle().Foreground(targetColor),
		lipgloss.NewStyle().Foreground(targetColor).Bold(true),
	}
)

func (t targetStatus) String(r *lipgloss.Renderer) string {
	target, output := targetStyle.target.Renderer(r), targetStyle.output.Renderer(r)

	outputStr := ""
	if t.output != "" {
		outputStr = output.Render("|" + t.output)
	}

	return target.Render(t.fqn) + outputStr + " " + t.status
}

func TargetOutputStatus(t specs.Specer, output string, status string) status.Statuser {
	if output == "" {
		output = "-"
	}
	return targetStatus{t.Spec().FQN, output, status}
}
