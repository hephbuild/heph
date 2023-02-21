package main

import (
	. "e2e/lib"
	"os"
	"path/filepath"
)

// This is a sanity test that running a target works which has broken cached locally
func main() {
	cache := MustV(TempDir())
	defer os.RemoveAll(cache)

	Must(ReplaceFile(".hephconfig.local", "<URI>", "file://"+cache+"/"))

	Must(CleanSetup())

	// Test zero cache run
	Must(Run("//:hello"))
	Must(ValidateCache("//:hello", []string{""}, false, false))

	cacheRoot := MustV(TargetCacheRoot("//:hello"))
	Must(os.Remove(filepath.Join(cacheRoot, "out_.tar.gz")))

	// Test zero cache run
	Must(Run("//:hello"))
	Must(ValidateCache("//:hello", []string{""}, false, false))
}
