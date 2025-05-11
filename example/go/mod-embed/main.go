package main

import (
	"embed"
	"fmt"
	"mod-embed/testutil"
)

//go:embed file.txt
var file string

//go:embed *.txt
var files embed.FS

//go:embed resource/file.txt
var resourceFile string

//go:embed resource/*.txt
var resourceFiles embed.FS

func main() {
	fmt.Println("hello")

	testutil.Assert(file, files, false)
	testutil.Assert(resourceFile, resourceFiles, true)
}
