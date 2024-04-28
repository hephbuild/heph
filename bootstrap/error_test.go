package bootstrap

import (
	"errors"
	"github.com/hephbuild/heph/worker2"
	"github.com/stretchr/testify/assert"
	"testing"
)

func TestCollectRootErrors(t *testing.T) {
	err := worker2.Error{
		ID:    2,
		Name:  "a2",
		State: worker2.ExecStateSkipped,
		Err: worker2.Error{
			ID:    1,
			Name:  "a1",
			State: worker2.ExecStateFailed,
			Err:   errors.New("sad beep bop"),
		},
	}

	errs := worker2.CollectRootErrors(err)

	assert.EqualValues(t, []error{
		worker2.Error{
			ID:    1,
			Name:  "a1",
			State: worker2.ExecStateFailed,
			Err:   errors.New("sad beep bop"),
		},
	}, errs)
}
