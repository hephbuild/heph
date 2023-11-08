package tuistatus

import (
	"context"
	"errors"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/utils/xtea"
)

func do(ctx context.Context, f func(ctx context.Context)) error {
	if !xtea.IsTerm() || !xtea.SingleflightTry() {
		f(ctx)
		return nil
	}

	defer xtea.SingleflightDone()

	h := &handler{}

	ctx = status.ContextWithHandler(ctx, h)

	m := model{
		log: xtea.NewLogModel(),
		h:   h,
		f: func() {
			f(ctx)
		},
	}

	err := xtea.RunModel(m, tea.WithInput(nil))
	if err != nil {
		return err
	}

	return nil
}

func DoE(ctx context.Context, f func(ctx context.Context) error) error {
	var ferr error
	err := do(ctx, func(ctx context.Context) {
		ferr = f(ctx)
	})

	return errors.Join(ferr, err)
}
