package hbuiltin

import (
	"github.com/hephbuild/heph/buildfiles"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils"
	"github.com/hephbuild/heph/utils/hash"
	"go.starlark.net/starlark"
)

func buildConfigUniverse(cfg *config.Config) starlark.StringDict {
	cfgd := &starlark.Dict{}

	cfgd.SetKey(starlark.String("version"), starlark.String(cfg.Version.String))

	profiles := starlark.NewList(nil)
	for _, profile := range cfg.Profiles {
		profiles.Append(starlark.String(profile))
	}
	cfgd.SetKey(starlark.String("profiles"), profiles)

	for k, v := range cfg.Extras {
		sv := utils.FromGo(v)
		cfgd.SetKey(starlark.String(k), sv)
	}

	cfgd.Freeze()

	return starlark.StringDict{
		"CONFIG": cfgd,
	}
}

func Bootstrap(opts Opts) buildfiles.RunOptions {
	configUniverse := buildConfigUniverse(opts.Config)
	predeclaredGlobalsOnce(configUniverse)
	universe := predeclared(predeclaredGlobals, configUniverse)

	return buildfiles.RunOptions{
		ThreadModifier: func(thread *starlark.Thread, pkg *packages.Package) {
			thread.SetLocal(optsKey, opts)
			thread.SetLocal(pkgKey, pkg)
		},
		UniverseFactory: func() starlark.StringDict {
			return universe
		},
		CacheDirPath: opts.Root.Home.Join("__BUILD").Abs(),
		BuildHash: func(h hash.Hash) {
			h.String(predeclaredHash)
		},
		Packages: opts.Pkgs,
		RootPkg: opts.Pkgs.GetOrCreate(packages.Package{
			Path: "",
			Root: opts.Root.Root,
		}),
	}
}

const optsKey = "__heph_opts"

type Opts struct {
	Pkgs           *packages.Registry
	Root           *hroot.State
	Config         *config.Config
	RegisterTarget func(t specs.Target) error
}

func getOpts(thread *starlark.Thread) Opts {
	pkg := thread.Local(optsKey).(Opts)

	return pkg
}

const pkgKey = "__heph_pkg"

func getPackage(thread *starlark.Thread) *packages.Package {
	pkg := thread.Local(pkgKey).(*packages.Package)

	if pkg == nil {
		panic("pkg is nil, not supposed to happen")
	}

	return pkg
}
