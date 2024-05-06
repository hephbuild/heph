package main

import (
	. "e2e/lib"
	"fmt"
)

// This tests that running remote cache works
func main() {
	cache := MustV(TempDir())
	defer RemoveAll(cache)
	tmp := MustV(TempDir())
	defer RemoveAll(tmp)

	Must(ReplaceFile(".hephconfig.local", "<URI>", "file://"+cache+"/"))
	Must(ReplaceFile(".hephconfig.local", "<TMP>", tmp))

	Must(CleanSetup())

	fmt.Println("Remote cache at ", cache)

	// Test zero cache run
	Must(RunO("//:without-out", RunOpts{NoInline: true}))
	Must(ValidateCache("//:without-out", nil, false, true, false))
	Must(ValidateRemoteCache(cache, "//:without-out", nil))
}
