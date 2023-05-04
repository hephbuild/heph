package main

import . "e2e/lib"

// This is a sanity test that running a target works
func main() {
	Must(CleanSetup())

	// Test zero cache run
	Must(Run("//:hello"))
	Must(ValidateCache("//:hello", []string{""}, false, false, true))
	hashInput1 := MustV(TargetCacheInputHash("//:hello"))

	Must(RmCache())

	// Test zero cache query out
	outputs := MustV(RunOutput("//:hello"))
	Must(ValidateCache("//:hello", []string{""}, false, false, true))
	Must(AssertFileContentEqual(outputs[0], "hello"))
	hashInput2 := MustV(TargetCacheInputHash("//:hello"))

	Must(AssertEqual(hashInput2, hashInput1))

	// Check full cache hit
	Must(Run("//:hello"))
	Must(ValidateCache("//:hello", []string{""}, false, false, true))
}
