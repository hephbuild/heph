package lib

import (
	"errors"
	"fmt"
	"os"
	"sync"
	"time"
)

// heph may have a startup time that is quite long, dur to compilation
var startupOffset time.Duration
var onceStartupOffset sync.Once

func computeStartupOffset() {
	start := time.Now()
	cmd := command("--version")
	b, err := cmd.CombinedOutput()
	if err != nil {
		panic(fmt.Errorf("%v: %w: %s", cmd.Args, err, b))
	}
	startupOffset = time.Since(start)
}

func WaitFileContentEqual(timeout time.Duration, p, expected string) error {
	return Wait(timeout, 500*time.Millisecond, func() (bool, error) {
		b, err := os.ReadFile(p)
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return false, err
		}

		return string(b) == expected, nil
	})
}

func Wait(timeout, delay time.Duration, f func() (bool, error)) error {
	onceStartupOffset.Do(computeStartupOffset)

	t := time.NewTimer(startupOffset + timeout)
	for {
		select {
		case <-t.C:
			return fmt.Errorf("wait timeout")
		default:
			done, err := f()
			if err != nil {
				return err
			}

			if done {
				return nil
			}

			time.Sleep(delay)
		}
	}
}
