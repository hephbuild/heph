package buildfiles

import (
	"context"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/hephbuild/heph/utils/xfs"
	"go.starlark.net/resolve"
	"go.starlark.net/starlark"
	"io/fs"
	"path/filepath"
	"sync"
)

func init() {
	resolve.AllowGlobalReassign = true
	resolve.AllowRecursion = true
}

type State struct {
	Patterns []string
	Ignore   []string
	Packages *packages.Registry
	files    []string

	cacheRunBuildFileCache maps.Map[string, starlark.StringDict]
	cacheRunBuildFileLocks maps.Map[string, *sync.Mutex]
	cacheLoads             maps.Map[string, starlark.StringDict]
}

func NewState(s State) *State {
	s.cacheRunBuildFileCache = maps.Map[string, starlark.StringDict]{}
	s.cacheRunBuildFileLocks = maps.Map[string, *sync.Mutex]{Default: func(k string) *sync.Mutex {
		return &sync.Mutex{}
	}}
	return &s
}

func (s *State) CollectFilesInRoot(ctx context.Context, root string) ([]string, error) {
	return s.collectFiles(ctx, root, nil)
}

func (s *State) CollectFiles(ctx context.Context, root string) ([]string, error) {
	return s.collectFiles(ctx, root, s.Ignore)
}

func (s *State) collectFiles(ctx context.Context, root string, ignore []string) ([]string, error) {
	done := log.TraceTimingDone("RunBuildFiles:walk")
	defer done()

	files := make([]string, 0)

	err := xfs.StarWalkAbs(ctx, root, s.Patterns[0], ignore, func(path string, d fs.DirEntry, err error) error {
		files = append(files, path)
		return nil
	})
	if err != nil {
		return nil, err
	}

	return files, nil
}

func (s *State) RunBuildFiles(ctx context.Context, files []string, options RunOptions) error {
	rootPkg := options.RootPkg
	rootAbs := rootPkg.Root.Abs()

	pkgs := make([]*packages.Package, 0, len(files))
	for _, file := range files {
		dir := filepath.Dir(file)

		relRoot, err := filepath.Rel(rootAbs, dir)
		if err != nil {
			return err
		}

		if relRoot == "." {
			relRoot = ""
		}

		pkg := s.Packages.GetOrCreate(rootPkg.Child(relRoot))

		pkgs = append(pkgs, pkg)

		s.files = append(s.files, file)
		pkg.SourceFiles = append(pkg.SourceFiles, file)
	}

	rctx := s.runContext(options)

	for _, pkg := range pkgs {
		err := rctx.runBuildFilesForPackage(pkg, nil)
		if err != nil {
			return err
		}
	}

	return nil
}

func (s *State) RunBuildFile(pkg *packages.Package, path string, options RunOptions) error {
	rctx := s.runContext(options)

	_, err := rctx.RunBuildFile(pkg, path, nil)
	return err
}

func (s *State) runContext(options RunOptions) runContext {
	return runContext{
		State:        s,
		RunOptions:   options,
		cacheGlobals: &s.cacheRunBuildFileCache,
		cacheLocks:   &s.cacheRunBuildFileLocks,
		cacheLoads:   &s.cacheLoads,
	}
}
