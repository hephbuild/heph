package main

import (
	. "e2e/lib"
	"path/filepath"
)

// This is a sanity test that running a target works which has broken cached locally
func main() {
	cache := MustV(TempDir())
	defer RemoveAll(cache)

	Must(ReplaceFile(".hephconfig.local", "<URI>", "file://"+cache+"/"))

	Must(CleanSetup())

	// Test zero cache run
	Must(Run("//:hello"))
	Must(ValidateCache("//:hello", []string{""}, false, true, false))

	cacheRoot := MustV(TargetCacheRoot("//:hello"))
	Must(RemoveAll(filepath.Join(cacheRoot, "out_.tar.gz")))

	// Test zero cache run
	Must(Run("//:hello"))
	Must(ValidateCache("//:hello", []string{""}, false, true, false))
}
