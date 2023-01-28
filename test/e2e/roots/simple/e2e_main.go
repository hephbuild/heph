package main

import . "e2e/lib"

func main() {
	Must(CleanSetup())

	// Test zero cache run
	Must(Run("//:hello"))
	Must(ValidateCache("//:hello", []string{""}))
	hashInput1 := MustV(TargetCacheInput("//:hello"))

	Must(RmCache())

	// Test zero cache query out
	outputs := MustV(RunOutput("//:hello"))
	Must(ValidateCache("//:hello", []string{""}))
	Must(FileContentEqual(outputs[0], "hello"))
	hashInput2 := MustV(TargetCacheInput("//:hello"))

	Must(AssertEqual(hashInput2, hashInput1))

	// Check full cache hit
	Must(Run("//:hello"))
	Must(ValidateCache("//:hello", []string{""}))
}
