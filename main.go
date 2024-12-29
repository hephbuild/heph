package main

import (
	"os"

	"github.com/hephbuild/hephv2/internal/cmd"
)

func main() {
	code := cmd.Execute()

	os.Exit(code)
}
