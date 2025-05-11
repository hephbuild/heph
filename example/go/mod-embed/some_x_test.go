package main_test

import (
	"embed"
	"mod-embed/testutil"
	"testing"
)

//go:embed file.txt
var fileTest string

//go:embed *.txt
var filesTest embed.FS

//go:embed resource/file.txt
var resourceFileTest string

//go:embed resource/*.txt
var resourceFilesTest embed.FS

func TestSanity(t *testing.T) {
	testutil.Assert(fileTest, filesTest, false)
	testutil.Assert(resourceFileTest, resourceFilesTest, true)
}
