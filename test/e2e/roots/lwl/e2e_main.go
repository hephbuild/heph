package main

import . "e2e/lib"

// This is a sanity test that running a target works
func main() {
	Must(CleanSetup())

	// Test zero cache run
	Must(Run("//:run_pack"))
}
