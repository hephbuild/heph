package main

import (
	. "e2e/lib"
	"fmt"
	"os"
	"path/filepath"
)

func Scenario(tgt string, outputs []string) {
	cache := MustV(TempDir())
	defer os.RemoveAll(cache)
	tmp := MustV(TempDir())
	defer os.RemoveAll(tmp)

	Must(ReplaceFile(".hephconfig.local", "<URI>", "file://"+cache+"/"))
	Must(ReplaceFile(".hephconfig.local", "<TMP>", tmp))

	Must(CleanSetup())

	fmt.Println("Remote cache at ", cache)

	touchrun := filepath.Join(tmp, "run")

	// Test zero cache run
	Must(Run(tgt))
	Must(ValidateCache(tgt, outputs, false, true, false))
	Must(ValidateRemoteCache(cache, tgt, outputs))
	hashInput1 := MustV(TargetCacheInputHash(tgt))
	runAt1 := MustV(FileModTime(touchrun))

	Must(RmCache())

	// Test remote cache run
	if len(outputs) > 0 {
		outputPaths := MustV(RunOutput(tgt))
		Must(ValidateCache(tgt, outputs, true, true, false))
		Must(AssertFileContentEqual(outputPaths[0], "hello"))
	} else {
		Must(Run(tgt))
		Must(ValidateCache(tgt, outputs, true, true, false))
	}
	hashInput2 := MustV(TargetCacheInputHash(tgt))
	runAt2 := MustV(FileModTime(touchrun))

	Must(AssertEqual(runAt2, runAt1))

	Must(AssertEqual(hashInput2, hashInput1))
}
