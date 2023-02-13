package lib

import (
	"fmt"
	"os"
	"path/filepath"
	"runtime"
)

func must(err error) {
	if err != nil {
		_, file, col, _ := runtime.Caller(2)
		file = filepath.Base(file)
		fmt.Fprintf(os.Stdout, "ERROR: must assertion failed at %v:%v:\n%v\n", file, col, err)
		os.Exit(1)
	}
}

func Must(err error) {
	must(err)
}

func MustV[T any](v T, err error) T {
	must(err)

	return v
}
