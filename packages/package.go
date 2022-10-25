package packages

import (
	"go.starlark.net/starlark"
	"heph/utils/fs"
)

type Package struct {
	Name        string
	FullName    string
	Root        fs.Path
	Globals     starlark.StringDict `json:"-" msgpack:"-"`
	SourceFiles SourceFiles
}

func (p Package) TargetPath(name string) string {
	return "//" + p.FullName + ":" + name
}

type SourceFile struct {
	Path string
}

type SourceFiles []*SourceFile

func (sf SourceFiles) Find(p string) *SourceFile {
	for _, file := range sf {
		if file.Path == p {
			return file
		}
	}

	return nil
}
