package main

import . "e2e/lib"

// This is a sanity test that bin/libs extracted from a docker container
// work without libs installed in the system
func main() {
	Must(CleanSetup())

	// Test zero cache run
	outputs := MustV(RunOutput("//:run_py"))
	Must(ValidateCache("//:run_py", []string{""}, false, false))
	Must(AssertFileContentEqual(outputs[0], "Hello, world"))
}
