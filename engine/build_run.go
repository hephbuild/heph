package engine

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/engine/buildfiles"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils"
	"github.com/hephbuild/heph/utils/hash"
	"go.starlark.net/starlark"
)

func (e *Engine) defaultRegisterTarget(spec targetspec.TargetSpec) error {
	l := e.TargetsLock.Get(spec.FQN)
	l.Lock()

	if t := e.Targets.Find(spec.FQN); t != nil {
		l.Unlock()

		if !t.TargetSpec.Equal(spec) {
			return fmt.Errorf("%v is already declared and does not equal the one defined in %v\n%s\n\n%s", spec.FQN, t.Source, t.Json(), spec.Json())
		}

		return nil
	}

	e.Targets.Add(&Target{
		Target: &tgt.Target{
			TargetSpec: spec,
		},
	})
	l.Unlock()

	return nil
}

type runBuildEngine struct {
	*Engine
	registerTarget func(targetspec.TargetSpec) error
	onceOptions    utils.Once[buildfiles.RunOptions]
}

func (e *runBuildEngine) config() starlark.StringDict {
	cfg := &starlark.Dict{}

	cfg.SetKey(starlark.String("version"), starlark.String(e.Config.Version.String))

	profiles := starlark.NewList(nil)
	for _, profile := range e.Config.Profiles {
		profiles.Append(starlark.String(profile))
	}
	profiles.Freeze()
	cfg.SetKey(starlark.String("profiles"), profiles)

	for k, v := range e.Config.Extras {
		sv := utils.FromGo(v)
		sv.Freeze()
		cfg.SetKey(starlark.String(k), sv)
	}

	cfg.Freeze()

	return starlark.StringDict{
		"CONFIG": cfg,
	}
}

func (e *runBuildEngine) runUtils() buildfiles.RunOptions {
	return e.onceOptions.MustDo(func() (buildfiles.RunOptions, error) {
		configUniverse := e.config()
		predeclaredGlobalsOnce(configUniverse)
		universe := predeclared(predeclaredGlobals, configUniverse)

		return buildfiles.RunOptions{
			ThreadModifier: func(thread *starlark.Thread) {
				thread.SetLocal("__engine", e)
			},
			UniverseFactory: func() starlark.StringDict {
				return universe
			},
			CacheDirPath: e.HomeDir.Join("__BUILD").Abs(),
			BuildHash: func(h hash.Hash) {
				h.String(predeclaredHash)
			},
			Packages: e.Packages,
			RootPkg: e.Packages.GetOrCreate(packages.Package{
				Path: "",
				Root: e.Root,
			}),
		}, nil
	})
}

func (e *runBuildEngine) runRootBuildFiles(ctx context.Context, rootName string, cfg config.Root) error {
	p, err := e.Packages.FetchRoot(ctx, rootName, cfg)
	if err != nil {
		return fmt.Errorf("fetch: %w", err)
	}

	opts := e.runUtils()
	opts.RootPkg = e.Packages.GetOrCreate(packages.Package{
		Path: rootName,
		Root: p,
	})

	return e.BuildFilesState.RunBuildFiles(opts)
}

func (e *runBuildEngine) runBuildFiles() error {
	opts := e.runUtils()

	return e.BuildFilesState.RunBuildFiles(opts)
}

func (e *runBuildEngine) runBuildFile(pkg *packages.Package, path string) error {
	opts := e.runUtils()

	return e.BuildFilesState.RunBuildFile(pkg, path, opts)
}
