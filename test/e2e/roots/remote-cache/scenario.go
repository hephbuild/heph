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

	Must(ReplaceFile(".hephconfig.local", "uri: <URI>", "uri: file://"+cache+"/"))

	tmp := MustV(TempDir())
	defer os.RemoveAll(tmp)

	SetDefaultRunOpts(RunOpts{Params: map[string]string{"tmp": tmp}})

	Must(CleanSetup())

	fmt.Println("Remote cache at ", cache)

	touchrun := filepath.Join(tmp, "run")

	// Test zero cache run
	Must(Run(tgt))
	Must(ValidateCache(tgt, outputs, false))
	Must(ValidateRemoteCache(cache, tgt, outputs))
	hashInput1 := MustV(TargetCacheInputHash(tgt))
	runAt1 := MustV(FileModTime(touchrun))

	Must(RmCache())

	// Test remote cache run
	outputPaths := MustV(RunOutput(tgt))
	Must(ValidateCache(tgt, outputs, true))
	if len(outputs) > 0 {
		Must(AssertFileContentEqual(outputPaths[0], "hello"))
	}
	hashInput2 := MustV(TargetCacheInputHash(tgt))
	runAt2 := MustV(FileModTime(touchrun))

	Must(AssertEqual(runAt2, runAt1))

	Must(AssertEqual(hashInput2, hashInput1))
}
