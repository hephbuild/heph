package xcontext

import (
	"context"
	"github.com/stretchr/testify/assert"
	"os"
	"testing"
)

func TestCauseErr_Is(t *testing.T) {
	err := causeErr{cause: SignalCause{sig: os.Interrupt}, err: context.Canceled}

	assert.ErrorIs(t, err, SignalCause{sig: os.Interrupt})
	assert.ErrorIs(t, err, context.Canceled)

	assert.ErrorAs(t, err, &SignalCause{sig: os.Interrupt})
	assert.ErrorAs(t, err, &context.Canceled)
}
