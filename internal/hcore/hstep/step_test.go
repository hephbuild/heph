package hstep

import (
	"context"
	"slices"
	"testing"

	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	"github.com/stretchr/testify/assert"
)

func TestSanity(t *testing.T) {
	ctx := t.Context()

	var steps []*corev1.Step
	ctx = ContextWithHandler(ctx, func(ctx context.Context, pbstep *corev1.Step) *corev1.Step {
		i := slices.IndexFunc(steps, func(s *corev1.Step) bool {
			return s.GetId() == pbstep.GetId()
		})
		if i < 0 {
			steps = append(steps, pbstep)
		} else {
			steps[i] = pbstep
		}

		return pbstep
	})

	step, ctx := New(ctx, "petting dogs")
	// do stuff

	assert.Equal(t, "petting dogs", steps[0].GetText())
	assert.Equal(t, corev1.Step_STATUS_RUNNING, steps[0].GetStatus())

	assert.Len(t, steps, 1)

	{ // in another deeper function
		step, ctx := New(ctx, "screaming at rats")

		assert.Len(t, steps, 2)

		// do stuff
		assert.Equal(t, steps[0].GetId(), steps[1].GetParentId())
		assert.Equal(t, "screaming at rats", steps[1].GetText())

		step.SetText("screaming loudly at rats")
		assert.Equal(t, "screaming loudly at rats", steps[1].GetText())

		_ = ctx
		step.Done()

		assert.Equal(t, corev1.Step_STATUS_COMPLETED.String(), steps[1].GetStatus().String())
	}

	step.Done()

	assert.Equal(t, corev1.Step_STATUS_COMPLETED.String(), steps[0].GetStatus().String())

	assert.Len(t, steps, 2)
}
