package main

import (
	. "e2e/lib"
	"time"
)

// This is a sanity test that codegen works
func main() {
	Must(CleanSetup())

	Must(WriteFile("file.txt", "12345"))
	Must(Run("//:hello"))
	Must(AssertFileContentEqual("out", "12345"))

	Must(WriteFile("file.txt", "123456789"))
	Must(Run("//:hello"))
	Must(AssertFileContentEqual("out", "123456789"))

	Must(WriteFile("file.txt", "12345"))
	Must(Run("//:hello"))
	Must(AssertFileContentEqual("out", "12345"))

	time.Sleep(time.Second)

	// Same size, different output
	Must(WriteFile("file.txt", "54321"))
	Must(Run("//:hello"))
	Must(AssertFileContentEqual("out", "54321"))
}
