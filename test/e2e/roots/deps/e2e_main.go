package main

import (
	. "e2e/lib"
	"strings"
)

func main() {
	Must(CleanSetup())

	// Test zero cache run
	Must(Run("//:run"))
	Must(ValidateCache("//:run", []string{""}))
	hashInput1 := MustV(TargetCacheInput("//:run"))

	Must(RmCache())

	// Test zero cache query out
	outputs := MustV(RunOutput("//:run"))
	Must(ValidateCache("//:run", []string{""}))
	expected := strings.TrimSpace(`
hello
#!/bin/sh
echo hello`)
	Must(FileContentEqual(outputs[0], expected))
	hashInput2 := MustV(TargetCacheInput("//:run"))

	Must(AssertEqual(hashInput2, hashInput1))

	// Check full cache hit
	Must(Run("//:run"))
	Must(ValidateCache("//:run", []string{""}))
}
