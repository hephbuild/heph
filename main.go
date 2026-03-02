package main

import (
	"os"
	"runtime"

	"github.com/hephbuild/heph/internal/cmd"
)

func main() {
	runtime.GOMAXPROCS(1)

	code := cmd.Execute()

	os.Exit(code)
}
