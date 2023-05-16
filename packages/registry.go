package packages

import (
	"fmt"
	"github.com/hephbuild/heph/config"
	fs2 "github.com/hephbuild/heph/utils/fs"
	"path"
	"sort"
	"strings"
	"sync"
)

type Registry struct {
	packagesMutex sync.RWMutex
	packagesm     map[string]*Package
	packages      []*Package

	RepoRoot       fs2.Path
	HomeRoot       fs2.Path
	Roots          map[string]config.Root
	RootsCachePath string
	fetchRootCache map[string]fs2.Path
	sorted         bool
}

func NewRegistry(r Registry) *Registry {
	r.packagesm = map[string]*Package{}
	r.fetchRootCache = map[string]fs2.Path{}

	return &r
}

func (e *Registry) All() []*Package {
	e.packagesMutex.Lock()
	if !e.sorted {
		sort.Slice(e.packages, func(i, j int) bool {
			return strings.Compare(e.packages[i].Path, e.packages[j].Path) < 0
		})
		e.sorted = true
	}
	e.packagesMutex.Unlock()

	return e.packages[:]
}

func (e *Registry) Get(path string) *Package {
	e.packagesMutex.RLock()
	defer e.packagesMutex.RUnlock()

	return e.packagesm[path]
}

func (e *Registry) GetOrCreate(spec Package) *Package {
	e.packagesMutex.Lock()
	defer e.packagesMutex.Unlock()

	ppath := spec.Path

	if pkg, ok := e.packagesm[ppath]; ok {
		return pkg
	}

	cpath := path.Clean(ppath)
	if cpath == "." {
		cpath = ""
	}

	if strings.HasPrefix(ppath, "./") || strings.HasSuffix(ppath, "/") || cpath != ppath {
		panic(fmt.Sprintf("path must be clean\npath: %v\nclean: %v | og: %v", ppath, cpath, ppath))
	}

	pkg := &Package{
		Path: ppath,
		Root: spec.Root,
	}

	e.packagesm[ppath] = pkg
	e.packages = append(e.packages, pkg)
	e.sorted = false

	return pkg
}
