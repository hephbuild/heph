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

type Paths []Path
type RelPaths []RelPath

func (ps Paths) WithRoot(root string) Paths {
	nps := make(Paths, 0, len(ps))

	for _, p := range ps {
		nps = append(nps, p.WithRoot(root))
	}

	return nps
}

type Path struct {
	root    string
	relRoot string
	abs     string
}

func (p Path) WithRoot(root string) Path {
	return Path{
		root:    root,
		relRoot: p.relRoot,
	}
}

func (p Path) Abs() string {
	if p.abs != "" {
		return p.abs
	}

	p.abs = filepath.Join(p.root, p.relRoot)

	return p.abs
}

func (p Path) RelRoot() string {
	return p.relRoot
}

func (p Path) Join(elem ...string) Path {
	return Path{
		root:    p.root,
		relRoot: filepath.Join(append([]string{p.relRoot}, elem...)...),
	}
}

type RootablePath interface {
	WithRoot(root string) Path
}

type RootablePaths interface {
	WithRoot(root string) Paths
}

type RelablePath interface {
	RelRoot() string
	RootablePath
}

type RelPath struct {
	relRoot string
}

func (p RelPath) RelRoot() string {
	return p.relRoot
}

func (p RelPath) WithRoot(root string) Path {
	return Path{
		root:    root,
		relRoot: p.relRoot,
	}
}
