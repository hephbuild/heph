package lib

import (
	"os"
)

func TempDir() (string, error) {
	return os.MkdirTemp("", "heph-e2e")
}
