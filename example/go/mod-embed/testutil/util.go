package testutil

import (
	"embed"
	"fmt"
	"io/fs"
)

func Assert(file string, files embed.FS, prefix bool) {
	if file != "hello\n" {
		panic(fmt.Sprintf("file is not hello: %q", file))
	}

	filename := "file.txt"
	if prefix {
		filename = "resource/" + filename
	}

	b, err := fs.ReadFile(files, filename)
	if err != nil {
		panic(err)
	}
	if string(b) != "hello\n" {
		panic(fmt.Sprintf("file is not hello: %q", string(b)))
	}
}
