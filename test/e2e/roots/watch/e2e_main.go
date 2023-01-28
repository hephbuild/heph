package main

import (
	. "e2e/lib"
	"os"
	"path/filepath"
	"time"
)

func main() {
	Must(CleanSetup())

	tmp := MustV(TempDir())
	defer os.RemoveAll(tmp)

	// Test zero cache run
	w := MustV(Watch("//:run", RunOpts{Params: map[string]string{"tmp": tmp}}))
	defer w.Kill()

	Must(WaitFileContentEqual(time.Second, filepath.Join(tmp, "hello.txt"), "hello"))

	Must(os.WriteFile("hello.txt", []byte("hello world"), os.ModePerm))

	Must(WaitFileContentEqual(time.Second, filepath.Join(tmp, "hello.txt"), "hello world"))
}
