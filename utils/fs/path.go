package fs

import (
	"encoding/json"
	"path/filepath"
)

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

func NewPath(root, relRoot string) Path {
	return Path{
		root:    root,
		relRoot: relRoot,
	}
}

func NewPathAbs(root, relRoot, abs string) Path {
	return Path{
		root:    root,
		relRoot: relRoot,
		abs:     abs,
	}
}

func (p *Path) MarshalJSON() ([]byte, error) {
	return json.Marshal(&struct {
		Root    string
		RelRoot string
		Abs     string `json:",omitempty"`
	}{
		Root:    p.root,
		RelRoot: p.relRoot,
		Abs:     p.abs,
	})
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

func (p Path) Root() string {
	return p.root
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

func NewRelPath(relRoot string) RelPath {
	return RelPath{relRoot: relRoot}
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
