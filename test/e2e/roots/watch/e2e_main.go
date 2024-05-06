package main

import (
	. "e2e/lib"
	"os"
	"path/filepath"
	"time"
)

// This is a sanity test that watching a target works
func main() {
	tmp := MustV(TempDir())
	defer RemoveAll(tmp)

	Must(ReplaceFile(".hephconfig.local", "<TMP>", tmp))

	Must(CleanSetup())

	// Test zero cache run
	w := MustV(Watch("//:run"))
	defer w.Kill()

	Must(WaitFileContentEqual(time.Second, filepath.Join(tmp, "hello.txt"), "hello"))

	Must(os.WriteFile("hello.txt", []byte("hello world"), os.ModePerm))

	Must(WaitFileContentEqual(time.Second, filepath.Join(tmp, "hello.txt"), "hello world"))
}
