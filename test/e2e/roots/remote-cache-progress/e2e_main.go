package main

import (
	. "e2e/lib"
	"fmt"
)

// Not really an e2e test, but rather a utility to try out the progress with remote cache
func main() {
	cache := MustV(TempDir())
	defer RemoveAll(cache)
	tmp := MustV(TempDir())
	defer RemoveAll(tmp)

	Must(ReplaceFile(".hephconfig.local", "<URI>", "file://"+cache+"/"))

	Must(CleanSetup())

	fmt.Println("Remote cache at ", cache)

	// Test zero cache run
	Must(Run("//:use-large-out"))

	Must(RmCache())

	// Test remote cache run
	Must(Run("//:use-large-out"))
}
