package packages

import (
	"github.com/hephbuild/heph/utils/xfs"
	"go.starlark.net/starlark"
	"path"
	"path/filepath"
)

type Package struct {
	Path        string
	Root        xfs.Path
	Globals     starlark.StringDict `json:"-" msgpack:"-"`
	SourceFiles []string
}

func (p *Package) Name() string {
	if p.Path == "" {
		return ""
	}

	return path.Base(p.Path)
}

func (p *Package) Addr() string {
	return "//" + p.Path
}

func (p *Package) TargetAddr(name string) string {
	return p.Addr() + ":" + name
}

func (p *Package) Child(childPath string) Package {
	if p.Path != "" && childPath != "" {
		childPath = p.Path + "/" + childPath
	} else if p.Path != "" {
		childPath = p.Path
	}
	return Package{
		Path: childPath,
		Root: p.Root.Join(filepath.FromSlash(childPath)),
	}
}

type SourceFile struct {
	Path string
}
