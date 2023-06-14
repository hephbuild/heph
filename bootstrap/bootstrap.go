package bootstrap

import (
	"context"
	"errors"
	"fmt"
	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/buildfiles"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/engine"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/hbuiltin"
	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/observability"
	obsummary "github.com/hephbuild/heph/observability/summary"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/platform"
	"github.com/hephbuild/heph/rcache"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/upgrade"
	"github.com/hephbuild/heph/utils/finalizers"
	"github.com/hephbuild/heph/worker"
	"os"
	"path/filepath"
	"strings"
)

func Getwd() (string, error) {
	if ecwd := os.Getenv("HEPH_CWD"); ecwd != "" {
		err := os.Chdir(ecwd)
		if err != nil {
			return "", err
		}

		return ecwd, nil
	}

	return os.Getwd()
}

func findRoot(cwd string) (string, error) {
	parts := strings.Split(cwd, string(filepath.Separator))
	for len(parts) > 0 {
		p := "/" + filepath.Join(parts...)

		if _, err := os.Stat(filepath.Join(p, ".hephconfig")); err == nil {
			return p, nil
		}

		parts = parts[:len(parts)-1]
	}

	return "", errors.New("root not found, are you running this command in the repo directory?")
}

type BootOpts struct {
	Profiles              []string
	Workers               int
	Params                map[string]string
	Summary               bool
	JaegerEndpoint        string
	DisableCloudTelemetry bool
	Pool                  *worker.Pool

	PostBootBase func(bs BaseBootstrap) error

	FlowID string
}

type BaseBootstrap struct {
	Cwd    string
	Root   *hroot.State
	Config *config.Config
}

func BootBase(ctx context.Context, opts BootOpts) (BaseBootstrap, error) {
	lipgloss.SetDefaultRenderer(log.Renderer())

	bs := BaseBootstrap{}

	cwd, err := Getwd()
	if err != nil {
		return bs, err
	}

	bs.Cwd = cwd

	rootPath, err := findRoot(cwd)
	if err != nil {
		return bs, fmt.Errorf("findRoot: %w", err)
	}

	root, err := hroot.NewState(rootPath)
	if err != nil {
		return bs, fmt.Errorf("hroot: %w", err)
	}

	bs.Root = root

	cfg, err := BuildConfig(root, opts.Profiles)
	if err != nil {
		return bs, fmt.Errorf("config: %w", err)
	}

	for k, v := range opts.Params {
		cfg.Params[k] = v
	}

	bs.Config = cfg

	err = upgrade.CheckAndUpdate(ctx, *cfg)
	if err != nil {
		return bs, fmt.Errorf("upgrade: %w", err)
	}

	if opts.PostBootBase != nil {
		err := opts.PostBootBase(bs)
		if err != nil {
			return bs, err
		}
	}

	return bs, nil
}

type Bootstrap struct {
	BaseBootstrap
	Finalizers        *finalizers.Finalizers
	Observability     *observability.Observability
	Cloud             Cloud
	Summary           *obsummary.Summary
	Pool              *worker.Pool
	Packages          *packages.Registry
	BuildFiles        *buildfiles.State
	Graph             *graph.State
	PlatformProviders []platform.PlatformProvider
}

func Boot(ctx context.Context, opts BootOpts) (Bootstrap, error) {
	bs := Bootstrap{}

	bbs, err := BootBase(ctx, opts)
	if err != nil {
		return bs, err
	}
	bs.BaseBootstrap = bbs

	root := bbs.Root
	cfg := bbs.Config

	fins := &finalizers.Finalizers{}
	bs.Finalizers = fins

	obs := observability.NewTelemetry()
	bs.Observability = obs

	err = setupJaeger(fins, obs, opts.JaegerEndpoint)
	if err != nil {
		return bs, fmt.Errorf("jaeger: %w", err)
	}

	cloud, err := setupHephcloud(ctx, root, cfg, fins, obs, !opts.DisableCloudTelemetry, opts.FlowID)
	if err != nil {
		return bs, fmt.Errorf("cloud: %w", err)
	}
	bs.Cloud = cloud

	if opts.Summary {
		sum := &obsummary.Summary{}
		obs.RegisterHook(sum)
		bs.Summary = sum
	}

	ctx, rootSpan := obs.SpanRoot(ctx)
	fins.RegisterWithErr(func(err error) {
		rootSpan.EndError(err)
	})

	pool := opts.Pool
	if pool == nil {
		pool = worker.NewPool(opts.Workers)
	}
	bs.Pool = pool

	pkgs := packages.NewRegistry(packages.Registry{
		Root:           root,
		RootsCachePath: root.Home.Join("roots").Abs(),
		Roots:          cfg.BuildFiles.Roots,
	})
	bs.Packages = pkgs

	buildfilesState := buildfiles.NewState(buildfiles.State{
		Ignore:   cfg.BuildFiles.Ignore,
		Packages: pkgs,
	})
	bs.BuildFiles = buildfilesState

	g, err := graph.NewState(root, cfg)
	if err != nil {
		return bs, fmt.Errorf("graph: %w", err)
	}
	bs.Graph = g

	{
		opts := hbuiltin.Bootstrap(hbuiltin.Opts{
			Pkgs:   pkgs,
			Root:   root,
			Config: cfg,
			RegisterTarget: func(spec targetspec.TargetSpec) error {
				return g.Register(spec)
			},
		})

		for name, cfg := range cfg.BuildFiles.Roots {
			opts := opts.Copy()

			p, err := pkgs.FetchRoot(ctx, name, cfg)
			if err != nil {
				return bs, fmt.Errorf("fetch: %w", err)
			}

			opts.RootPkg = pkgs.GetOrCreate(packages.Package{
				Path: name,
				Root: p,
			})

			err = buildfilesState.RunBuildFiles(opts)
			if err != nil {
				return bs, fmt.Errorf("buildfiles: root %v: %w", name, err)
			}
		}

		err := buildfilesState.RunBuildFiles(opts)
		if err != nil {
			return bs, fmt.Errorf("buildfiles: %w", err)
		}
	}

	bs.PlatformProviders = platform.Bootstrap(cfg)

	return bs, nil
}

func BootEngine(ctx context.Context, bs Bootstrap) (*engine.Engine, error) {
	localCache, err := engine.NewState(bs.Root, bs.Graph, bs.Observability)
	if err != nil {
		return nil, err
	}

	var getFlowId func() string
	if hook := bs.Cloud.Hook; hook != nil {
		getFlowId = func() string {
			return hook.FlowId
		}
	}

	e := engine.New(engine.Engine{
		Cwd:                    bs.Cwd,
		Root:                   bs.Root,
		Config:                 bs.Graph.Config,
		Observability:          bs.Observability,
		GetFlowID:              getFlowId,
		PlatformProviders:      bs.PlatformProviders,
		LocalCache:             localCache,
		RemoteCacheHints:       &rcache.HintStore{},
		Packages:               bs.Packages,
		BuildFilesState:        bs.BuildFiles,
		Graph:                  bs.Graph,
		Pool:                   bs.Pool,
		Finalizers:             &finalizers.Finalizers{},
		DisableNamedCacheWrite: false,
		Targets:                nil, // Will be set as part of engine.New
	})

	bs.Finalizers.RegisterWithErr(func(err error) {
		e.Finalizers.Run(err)
	})

	localCache.Targets = e.Targets

	if bs.Config.Engine.InstallTools {
		err = e.InstallTools(ctx)
		if err != nil {
			return nil, err
		}
	}

	return e, nil
}

type EngineBootstrap struct {
	Bootstrap
	Engine *engine.Engine
}

func BootWithEngine(ctx context.Context, opts BootOpts) (EngineBootstrap, error) {
	ebs := EngineBootstrap{}

	bs, err := Boot(ctx, opts)
	if err != nil {
		return ebs, err
	}
	ebs.Bootstrap = bs

	e, err := BootEngine(ctx, bs)
	if err != nil {
		return ebs, err
	}
	ebs.Engine = e

	return ebs, nil
}
