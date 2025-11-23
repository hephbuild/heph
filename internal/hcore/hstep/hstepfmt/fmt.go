package hstepfmt

import (
	"fmt"
	"time"

	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/internal/htime"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
)

var runningStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("4"))
var okStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("2"))
var koStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("1"))

func FormatStatus(renderer *lipgloss.Renderer, step *corev1.Step) string {
	switch step.GetStatus() {
	case corev1.Step_STATUS_RUNNING:
		return runningStyle.Renderer(renderer).Render("☲")

	case corev1.Step_STATUS_COMPLETED:
		if step.GetError() {
			return koStyle.Renderer(renderer).Render("✘")
		}

		return okStyle.Renderer(renderer).Render("✔")
	case corev1.Step_STATUS_UNSPECIFIED:
	}

	return " "
}

func FormatDuration(step *corev1.Step) string {
	var t time.Duration
	switch step.GetStatus() {
	case corev1.Step_STATUS_RUNNING:
		t = time.Since(step.GetStartedAt().AsTime())
	case corev1.Step_STATUS_COMPLETED:
		t = step.GetCompletedAt().AsTime().Sub(step.GetStartedAt().AsTime())
	case corev1.Step_STATUS_UNSPECIFIED:
	}

	if t <= 0 {
		return ""
	}

	return htime.FormatFixedWidthDuration(t)
}

func Format(renderer *lipgloss.Renderer, step *corev1.Step, interactive bool) string {
	d := FormatDuration(step)

	if d != "" {
		return fmt.Sprintf("%s %.5s %s", FormatStatus(renderer, step), d, step.GetText())
	}

	return fmt.Sprintf("%s %s", FormatStatus(renderer, step), step.GetText())
}
