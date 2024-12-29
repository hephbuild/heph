package hstep

import (
	"context"
	"slices"
	"testing"

	corev1 "github.com/hephbuild/hephv2/plugin/gen/heph/core/v1"
	"github.com/stretchr/testify/assert"
)

func TestSanity(t *testing.T) {
	ctx := context.Background()

	var steps []*corev1.Step
	ctx = ContextWithHandler(ctx, func(ctx context.Context, pbstep *corev1.Step) *corev1.Step {
		if !slices.ContainsFunc(steps, func(s *corev1.Step) bool {
			return s.GetId() == pbstep.GetId()
		}) {
			steps = append(steps, pbstep)
		}

		return pbstep
	})

	step, ctx := New(ctx, "petting dogs")
	// do stuff

	assert.Equal(t, "petting dogs", steps[0].GetText())
	assert.Equal(t, corev1.Step_STATUS_RUNNING, steps[0].GetStatus())

	{ // in another deeper function
		step, ctx := New(ctx, "screaming at rats")
		// do stuff
		assert.Equal(t, steps[0].GetId(), steps[1].GetParentId())
		assert.Equal(t, "screaming at rats", steps[1].GetText())

		step.SetText("screaming loudly at rats")
		assert.Equal(t, "screaming loudly at rats", steps[1].GetText())

		_ = ctx
		step.Done()
	}

	step.Done()

	assert.Equal(t, corev1.Step_STATUS_COMPLETED, steps[0].GetStatus())

	assert.Len(t, steps, 2)
}
