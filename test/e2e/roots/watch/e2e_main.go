package main

import (
	. "e2e/lib"
	"os"
	"path/filepath"
	"time"
)

// This is a sanity test that watching a target works
func main() {
	Must(CleanSetup())

	tmp := MustV(TempDir())
	defer os.RemoveAll(tmp)

	SetDefaultRunOpts(RunOpts{Params: map[string]string{"tmp": tmp}})

	// Test zero cache run
	w := MustV(Watch("//:run"))
	defer w.Kill()

	Must(WaitFileContentEqual(time.Second, filepath.Join(tmp, "hello.txt"), "hello"))

	Must(os.WriteFile("hello.txt", []byte("hello world"), os.ModePerm))

	Must(WaitFileContentEqual(time.Second, filepath.Join(tmp, "hello.txt"), "hello world"))
}
