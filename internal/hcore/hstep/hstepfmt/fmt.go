package hstepfmt

import (
	"fmt"
	"time"

	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/hephv2/internal/htime"
	corev1 "github.com/hephbuild/hephv2/plugin/gen/heph/core/v1"
)

var runningStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("4"))
var okStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("2"))
var koStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("1"))

func Format(renderer *lipgloss.Renderer, step *corev1.Step, interactive bool) string {
	var t time.Duration

	status := " "
	switch step.GetStatus() {
	case corev1.Step_STATUS_RUNNING:
		status = runningStyle.Renderer(renderer).Render("☲")
		if interactive {
			t = time.Since(step.GetStartedAt().AsTime())
		}
	case corev1.Step_STATUS_COMPLETED:
		status = okStyle.Renderer(renderer).Render("✔")
		if step.GetError() {
			status = koStyle.Renderer(renderer).Render("✘")
		}
		t = step.GetCompletedAt().AsTime().Sub(step.GetStartedAt().AsTime())
	case corev1.Step_STATUS_UNSPECIFIED:
	}

	if t > 0 {
		return fmt.Sprintf("%v %.5s %v", status, htime.FormatFixedWidthDuration(t), step.GetText())
	}

	return fmt.Sprintf("%v %v", status, step.GetText())
}
