package main

import (
	"heph/cmd"
	"os"
)

func main() {
	if len(os.Args) > 1 && os.Args[1] == "sandbox" {
		mainSandbox()
		os.Exit(1)
	}

	cmd.Execute()
}
