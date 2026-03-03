package main

import (
	"os"

	"github.com/hephbuild/heph/internal/cmd"
)

func main() {
	code := cmd.Execute()

	os.Exit(code)
}
