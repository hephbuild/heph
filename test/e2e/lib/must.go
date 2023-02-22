package lib

import (
	"fmt"
	"os"
	"path/filepath"
	"runtime"
)

func Fail(s string) {
	_, file, col, _ := runtime.Caller(1)
	file = filepath.Base(file)
	fmt.Fprint(os.Stdout, fmt.Sprintf("%v at %v:%v\n", s, file, col))
	os.Exit(1)
}

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
