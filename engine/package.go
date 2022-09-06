package engine

import (
	"go.starlark.net/starlark"
	"path/filepath"
)

type Package struct {
	Name        string
	FullName    string
	Root        Path
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

type Path struct {
	Root    string
	RelRoot string
}

func (p Path) Abs() string {
	return filepath.Join(p.Root, p.RelRoot)
}

func (p Path) Join(elem ...string) Path {
	return Path{
		Root:    p.Root,
		RelRoot: filepath.Join(append([]string{p.RelRoot}, elem...)...),
	}
}
