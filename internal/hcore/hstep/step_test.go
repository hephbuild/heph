package hstep

import (
	"context"
	"github.com/hephbuild/hephv2/plugin/gen/heph/core/v1"
	"github.com/stretchr/testify/assert"
	"slices"
	"testing"
)

func TestSanity(t *testing.T) {
	ctx := context.Background()

	var steps []*corev1.Step
	ctx = ContextWithHandler(ctx, func(ctx context.Context, pbstep *corev1.Step) *corev1.Step {
		if !slices.ContainsFunc(steps, func(s *corev1.Step) bool {
			return s.Id == pbstep.Id
		}) {
			steps = append(steps, pbstep)
		}

		return pbstep
	})

	step, ctx := New(ctx, "petting dogs")
	// do stuff

	assert.Equal(t, "petting dogs", steps[0].Text)
	assert.Equal(t, corev1.Step_STATUS_RUNNING, steps[0].Status)

	{ // in another deeper function
		step, ctx := New(ctx, "screaming at rats")
		// do stuff
		assert.Equal(t, steps[0].Id, steps[1].ParentId)
		assert.Equal(t, "screaming at rats", steps[1].Text)

		step.SetText("screaming loudly at rats")
		assert.Equal(t, "screaming loudly at rats", steps[1].Text)

		_ = ctx
		step.Done()
	}

	step.Done()

	assert.Equal(t, corev1.Step_STATUS_COMPLETED, steps[0].Status)

	assert.Len(t, steps, 2)

}
