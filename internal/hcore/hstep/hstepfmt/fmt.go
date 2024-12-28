package hstepfmt

import (
	"fmt"
	"github.com/hephbuild/hephv2/internal/htime"
	corev1 "github.com/hephbuild/hephv2/plugin/gen/heph/core/v1"
	"time"
)

func Format(step *corev1.Step, interactive bool) string {
	var t time.Duration

	status := " "
	switch step.Status {
	case corev1.Step_STATUS_RUNNING:
		status = "☲"
		if interactive {
			t = time.Since(step.StartedAt.AsTime())
		}
	case corev1.Step_STATUS_COMPLETED:
		status = "✔"
		t = step.CompletedAt.AsTime().Sub(step.StartedAt.AsTime())
	}

	if t > 0 {
		return fmt.Sprintf("%v %.5s %v", status, htime.FormatFixedWidthDuration(t), step.Text)
	}

	return fmt.Sprintf("%v %v", status, step.Text)
}
