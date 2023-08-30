package packages

import (
	"fmt"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/utils/xfs"
	"golang.org/x/exp/slices"
	"path"
	"strings"
	"sync"
)

type Registry struct {
	packagesMutex sync.RWMutex
	packagesm     map[string]*Package
	packages      []*Package

	Root           *hroot.State
	Roots          map[string]config.Root
	RootsCachePath string
	fetchRootCache map[string]xfs.Path
	sorted         bool
}

func NewRegistry(r Registry) *Registry {
	r.packagesm = map[string]*Package{}
	r.fetchRootCache = map[string]xfs.Path{}

	return &r
}

func (e *Registry) All() []*Package {
	e.packagesMutex.Lock()
	if !e.sorted {
		slices.SortFunc(e.packages, func(a, b *Package) int {
			return strings.Compare(a.Path, b.Path)
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
