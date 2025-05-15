package termui

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"os"
	"sync"
)

func NewNonInteractive(ctx context.Context, f RunFunc) error {
	var mu sync.Mutex
	return f(ctx, func(f hbbtexec.ExecFunc) error {
		if !mu.TryLock() {
			return fmt.Errorf("two concurrent interractive exec detected")
		}
		defer mu.Unlock()

		var restore func()
		makeRaw := sync.OnceValue(func() error {
			var err error
			restore, err = hbbtexec.MakeRaw(os.Stdin)
			return err
		})
		defer func() {
			if restore != nil {
				restore()
			}
		}()

		return f(hbbtexec.RunArgs{
			Stdin:   os.Stdin,
			Stdout:  os.Stderr,
			Stderr:  os.Stdout,
			MakeRaw: makeRaw,
		})
	})
}
