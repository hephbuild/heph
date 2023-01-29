package main

import (
	. "e2e/lib"
	"os"
	"path/filepath"
)

// This tests that changing when running a dependency A, changing a dependency C of a
// dependency B does not trigger a rerun when the output of B is the same
func main() {
	tmp := MustV(TempDir())
	defer os.RemoveAll(tmp)

	SetDefaultRunOpts(RunOpts{Params: map[string]string{"tmp": tmp}})

	Must(CleanSetup())

	touchhello := filepath.Join(tmp, "hello")
	touchrun := filepath.Join(tmp, "run")
	hellotxt := filepath.Join(tmp, "hello.txt")

	// Test zero cache run
	Must(Run("//:run"))
	Must(AssertFileContentEqual(hellotxt, "hello1"))
	helloAt1 := MustV(FileModTime(touchhello))
	runAt1 := MustV(FileModTime(touchrun))
	hashOutHello1 := MustV(TargetCacheOutputHash("//:hello", ""))

	Must(ReWriteFile("hello2.txt", "new stuff"))

	Must(Run("//:run"))
	helloAt2 := MustV(FileModTime(touchhello))
	runAt2 := MustV(FileModTime(touchrun))
	hashOutHello2 := MustV(TargetCacheOutputHash("//:hello", ""))

	Must(AssertEqual(hashOutHello2, hashOutHello1))

	Must(AssertNotEqual(helloAt2, helloAt1))
	Must(AssertEqual(runAt2, runAt1))

	Must(AssertFileContentEqual(hellotxt, "hello1"))
}
