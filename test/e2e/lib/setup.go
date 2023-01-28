package lib

import (
	"os"
)

func RmHome() error {
	cache, err := HomePath()
	if err != nil {
		return err
	}

	return os.RemoveAll(cache)
}

func CleanSetup() error {
	err := RmHome()
	if err != nil {
		return err
	}

	return nil
}
