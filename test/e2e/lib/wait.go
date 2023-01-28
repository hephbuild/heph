package lib

import (
	"errors"
	"fmt"
	"os"
	"time"
)

func WaitFileContentEqual(timeout time.Duration, p, expected string) error {
	return Wait(timeout, func() (bool, error) {
		b, err := os.ReadFile(p)
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return false, err
		}

		return string(b) == expected, nil
	})
}

func Wait(timeout time.Duration, f func() (bool, error)) error {
	t := time.NewTimer(timeout)
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
		}
	}
}
