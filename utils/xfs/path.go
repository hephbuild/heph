package xfs

import (
	"encoding/json"
	"path/filepath"
	"sort"
	"sync"
)

type Paths []Path

func (ps Paths) Sort() {
	sort.Slice(ps, func(i, j int) bool {
		return ps[i].RelRoot() < ps[j].RelRoot()
	})
}

func (ps Paths) WithRoot(root string) Paths {
	nps := make(Paths, 0, len(ps))

	for _, p := range ps {
		nps = append(nps, p.WithRoot(root))
	}

	return nps
}

type RelPaths []RelPath

func (ps RelPaths) Sort() {
	sort.Slice(ps, func(i, j int) bool {
		return ps[i].RelRoot() < ps[j].RelRoot()
	})
}

type Path struct {
	root    string
	relRoot string
	abs     string
}

func NewPath(root, relRoot string) Path {
	return NewPathAbs(root, relRoot, "")
}

func NewPathAbs(root, relRoot, abs string) Path {
	if relRoot == "." {
		relRoot = ""
	}

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
		Abs     string
	}{
		Root:    p.root,
		RelRoot: p.relRoot,
		Abs:     p.abs,
	})
}

func (p *Path) UnmarshalJSON(b []byte) error {
	var data struct {
		Root    string
		RelRoot string
		Abs     string
	}
	err := json.Unmarshal(b, &data)
	if err != nil {
		return err
	}

	*p = Path{
		root:    data.Root,
		relRoot: data.RelRoot,
		abs:     data.Abs,
	}

	return nil
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

var joinsPoolSize = 10
var joinsPool = sync.Pool{New: func() any {
	return make([]string, joinsPoolSize)
}}

func (p Path) RelRoot() string {
	return p.relRoot
}

func (p Path) Join(elem ...string) Path {
	var relRoot string
	if len(elem) > 0 {
		endIndex := len(elem) + 1

		var gjoins []string
		if endIndex < joinsPoolSize {
			gjoins = joinsPool.Get().([]string)
			defer joinsPool.Put(gjoins)
		} else {
			gjoins = make([]string, endIndex+1)
		}

		gjoins[0] = p.relRoot
		copy(gjoins[1:], elem)

		relRoot = filepath.Join(gjoins[:endIndex]...)
	} else {
		relRoot = p.relRoot
	}
	return Path{
		root:    p.root,
		relRoot: relRoot,
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