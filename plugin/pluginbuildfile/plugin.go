package pluginbuildfile

import (
	"context"
	"errors"
	"fmt"
	"maps"
	"path/filepath"
	"strings"

	"github.com/hephbuild/heph/internal/hsingleflight"
	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/lib/tref"
	"go.starlark.net/starlarkstruct"

	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hstarlark"
	"github.com/hephbuild/heph/lib/pluginsdk"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"go.starlark.net/starlark"
	"go.starlark.net/syntax"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/known/structpb"
)

type Options struct {
	Patterns []string
}

type Plugin struct {
	Options
	repoRoot    hfs.FS
	cacheget    hsingleflight.GroupMemContext[string, *pluginv1.TargetSpec]
	cacherunpkg CacheRunpkg
}

type Hooks struct {
	onTarget        onTargetFunc
	onProviderState onProviderStateFunc
}

var _ pluginsdk.Provider = (*Plugin)(nil)

const Name = "buildfile"

func New(fs hfs.FS, cfg Options) *Plugin {
	return &Plugin{
		Options:  cfg,
		repoRoot: fs,
	}
}

func (p *Plugin) Config(ctx context.Context, req *pluginv1.ProviderConfigRequest) (*pluginv1.ProviderConfigResponse, error) {
	return pluginv1.ProviderConfigResponse_builder{
		Name: htypes.Ptr(Name),
	}.Build(), nil
}

func (p *Plugin) Probe(ctx context.Context, req *pluginv1.ProbeRequest) (*pluginv1.ProbeResponse, error) {
	var states []*pluginv1.ProviderState
	_, err := p.runPkg(ctx, hfs.At(p.repoRoot, req.GetPackage()).Path(), Hooks{
		onProviderState: func(ctx context.Context, payload OnProviderStatePayload) error {
			if payload.Provider == Name {
				return nil
			}

			state := map[string]*structpb.Value{}
			for k, v := range payload.Args {
				v := hstarlark.FromStarlark(v)

				pv, err := structpb.NewValue(v)
				if err != nil {
					return err
				}

				state[k] = pv
			}

			states = append(states, pluginv1.ProviderState_builder{
				Package:  htypes.Ptr(payload.Package),
				Provider: htypes.Ptr(payload.Provider),
				State:    state,
			}.Build())

			return nil
		},
	})
	if err != nil {
		return nil, err
	}

	return pluginv1.ProbeResponse_builder{
		States: states,
	}.Build(), nil
}

func (p *Plugin) List(ctx context.Context, req *pluginv1.ListRequest) (pluginsdk.HandlerStreamReceive[*pluginv1.ListResponse], error) {
	return pluginsdk.NewChanHandlerStreamFunc(func(send func(*pluginv1.ListResponse) error) error {
		_, err := p.runPkg(ctx, req.GetPackage(), Hooks{
			onTarget: func(ctx context.Context, payload OnTargetPayload) error {
				spec, err := p.toTargetSpec(ctx, payload)
				if err != nil {
					return err
				}

				err = send(pluginv1.ListResponse_builder{
					Spec: proto.ValueOrDefault(spec),
				}.Build())

				return err
			},
		})

		return err
	}), nil
}

func (p *Plugin) runPkg(ctx context.Context, pkg string, hooks Hooks) (starlark.StringDict, error) {
	return p.cacherunpkg.Singleflight(ctx, pkg, hooks, func(hooks Hooks) (starlark.StringDict, error) {
		return p.runPkgInner(ctx, pkg, hooks)
	})
}

func (p *Plugin) runPkgInner(ctx context.Context, pkg string, hooks Hooks) (starlark.StringDict, error) {
	out := starlark.StringDict{}

	fs := hfs.At(p.repoRoot, pkg)
	for _, pattern := range p.Patterns {
		err := hfs.Glob(ctx, fs, pattern, nil, func(path string, d hfs.DirEntry) error {
			f, err := hfs.Open(fs, path)
			if err != nil {
				return err
			}
			defer f.Close()

			res, err := p.runFile(ctx, pkg, f, hooks)
			if err != nil {
				return err
			}

			maps.Copy(out, res)

			return nil
		})
		if err != nil {
			return nil, err
		}
	}

	out.Freeze()

	return out, nil
}

type BuiltinFunc = func(
	thread *starlark.Thread,
	fn *starlark.Builtin,
	args starlark.Tuple,
	kwargs []starlark.Tuple,
) (starlark.Value, error)

func (p *Plugin) file(name string) BuiltinFunc {
	return func(
		thread *starlark.Thread,
		fn *starlark.Builtin,
		args starlark.Tuple,
		kwargs []starlark.Tuple,
	) (starlark.Value, error) {
		execCtx := getExecContext(thread)

		var pattern string
		var exclude hstarlark.List[string]
		if err := starlark.UnpackArgs(
			name, args, kwargs,
			"pattern", &pattern,
			"exclude?", &exclude,
		); err != nil {
			return nil, err
		}

		base, pattern := hfs.GlobSplit(pattern)

		if strings.HasPrefix(base, "/") {
			return starlark.String(tref.FormatGlob(base, pattern, exclude)), nil
		}

		return starlark.String(tref.FormatGlob(tref.JoinPackage(execCtx.Package, base), pattern, exclude)), nil
	}
}

func (p *Plugin) get_pkg() BuiltinFunc {
	return func(
		thread *starlark.Thread,
		fn *starlark.Builtin,
		args starlark.Tuple,
		kwargs []starlark.Tuple,
	) (starlark.Value, error) {
		execCtx := getExecContext(thread)

		return starlark.String(execCtx.Package), nil
	}
}

func (p *Plugin) builtinTarget() BuiltinFunc {
	return func(
		thread *starlark.Thread,
		fn *starlark.Builtin,
		args starlark.Tuple,
		kwargs []starlark.Tuple,
	) (starlark.Value, error) {
		execCtx := getExecContext(thread)

		var fkwargs []starlark.Tuple
		var otherkwargs = map[string]starlark.Value{}
		for _, item := range kwargs {
			name, arg := item[0].(starlark.String), item[1] //nolint:errcheck

			switch name {
			case "name", "driver", "labels", "transitive":
				fkwargs = append(fkwargs, item)
			default:
				otherkwargs[string(name)] = arg
			}
		}

		var tspec TargetSpec
		tspec.Driver = "bash"

		if err := starlark.UnpackArgs(
			"target", args, fkwargs,
			"name", &tspec.Name,
			"driver?", &tspec.Driver,
			"labels?", &tspec.Labels,
			"transitive?", &tspec.Transitive,
		); err != nil {
			if tspec.Name != "" {
				return nil, fmt.Errorf("%v: %w", tspec.Name, err)
			}

			return nil, err
		}

		payload := OnTargetPayload{
			Name:       tspec.Name,
			Package:    execCtx.Package,
			Driver:     tspec.Driver,
			Labels:     tspec.Labels,
			Transitive: tspec.Transitive,
			Args:       otherkwargs,
		}

		if payload.Name == "" {
			return nil, errors.New("missing name")
		}

		err := execCtx.OnTarget(execCtx.Ctx, payload)
		if err != nil {
			return nil, err
		}

		return starlark.String(tref.Format(tref.New(execCtx.Package, payload.Name, nil))), nil
	}
}

func (p *Plugin) builtinProviderState() BuiltinFunc {
	return func(
		thread *starlark.Thread,
		fn *starlark.Builtin,
		args starlark.Tuple,
		kwargs []starlark.Tuple,
	) (starlark.Value, error) {
		execCtx := getExecContext(thread)

		var fkwargs []starlark.Tuple
		var otherkwargs = map[string]starlark.Value{}
		for _, item := range kwargs {
			name, arg := item[0].(starlark.String), item[1] //nolint:errcheck

			switch name {
			case "provider":
				fkwargs = append(fkwargs, item)
			default:
				otherkwargs[string(name)] = arg
			}
		}

		payload := OnProviderStatePayload{
			Package: execCtx.Package,
			Args:    otherkwargs,
		}
		if err := starlark.UnpackArgs(
			"provider_state", args, fkwargs,
			"provider?", &payload.Provider,
		); err != nil {
			return nil, err
		}

		if payload.Provider == "" {
			return nil, errors.New("missing provider")
		}

		err := execCtx.OnProviderState(execCtx.Ctx, payload)
		if err != nil {
			return nil, err
		}

		return starlark.None, nil
	}
}

func (p *Plugin) runFile(ctx context.Context, pkg string, file hfs.File, hooks Hooks) (starlark.StringDict, error) {
	return p.cacherunpkg.Singleflight(ctx, file.Name(), hooks, func(hooks Hooks) (starlark.StringDict, error) {
		return p.runFileInner(ctx, pkg, file, hooks)
	})
}

func (p *Plugin) runFileInner(ctx context.Context, pkg string, file hfs.File, hooks Hooks) (starlark.StringDict, error) {
	if hooks.onTarget == nil {
		hooks.onTarget = func(ctx context.Context, payload OnTargetPayload) error {
			return nil
		}
	}
	if hooks.onProviderState == nil {
		hooks.onProviderState = func(ctx context.Context, payload OnProviderStatePayload) error {
			return nil
		}
	}
	universe := starlark.StringDict{
		"target":         starlark.NewBuiltin("target", p.builtinTarget()),
		"struct":         starlark.NewBuiltin("struct", starlarkstruct.Make),
		"glob":           starlark.NewBuiltin("glob", p.file("glob")),
		"file":           starlark.NewBuiltin("file", p.file("file")),
		"provider_state": starlark.NewBuiltin("provider_state", p.builtinProviderState()),
		"get_pkg":        starlark.NewBuiltin("get_pkg", p.get_pkg()),
	}
	prog, err := p.buildFile(ctx, file, universe)
	if err != nil {
		return nil, err
	}

	thread := &starlark.Thread{
		Print: func(thread *starlark.Thread, msg string) {
			hlog.From(ctx).Info(msg)
		},
		Load: func(thread *starlark.Thread, module string) (starlark.StringDict, error) {
			switch {
			case strings.HasPrefix(module, "//"):
				rest, _ := strings.CutPrefix(module, "//")

				res, err := p.runPkg(ctx, rest, hooks)
				if err != nil {
					return nil, fmt.Errorf("%v: %w", module, err)
				}

				return res, nil
			case strings.HasPrefix(module, "./"):
				execCtx := getExecContext(thread)

				f, err := hfs.Open(p.repoRoot, filepath.Join(tref.ToOSPath(execCtx.Package), module))
				if err != nil {
					return nil, err
				}
				defer f.Close()

				res, err := p.runFile(ctx, execCtx.Package, f, hooks)
				if err != nil {
					return nil, fmt.Errorf("%v: %w", module, err)
				}

				return res, nil
			case strings.HasPrefix(module, "@"):
				return nil, errors.New("import function from plugin not implemented")
			default:
				return nil, fmt.Errorf("unsupported module %q", module)
			}
		},
	}
	setExecContext(thread, execContext{
		Ctx:             ctx,
		Package:         pkg,
		OnTarget:        hooks.onTarget,
		OnProviderState: hooks.onProviderState,
	})

	res, err := prog.Init(thread, universe)
	if err != nil {
		var eerr *starlark.EvalError
		if errors.As(err, &eerr) {
			return nil, fmt.Errorf("%v:\n%v", eerr.Msg, eerr.Backtrace())
		}
		return nil, err
	}
	res.Freeze()

	return res, nil
}

func (p *Plugin) buildFile(ctx context.Context, file hfs.File, universe starlark.StringDict) (*starlark.Program, error) {
	opts := &syntax.FileOptions{
		While:             false,
		TopLevelControl:   true,
		GlobalReassign:    false,
		LoadBindsGlobally: false,
		Recursion:         true,
	}
	_, prog, err := starlark.SourceProgramOptions(opts, file.Name(), file, universe.Has)
	if err != nil {
		return nil, err
	}

	return prog, nil
}

func (p *Plugin) Get(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.GetResponse, error) {
	spec, err, _ := p.cacheget.Do(ctx, tref.Format(req.GetRef()), func(ctx context.Context) (*pluginv1.TargetSpec, error) {
		return p.getInner(ctx, req)
	})
	if err != nil {
		return nil, err
	}

	return pluginv1.GetResponse_builder{
		Spec: spec,
	}.Build(), nil
}

func (p *Plugin) toTargetSpec(ctx context.Context, payload OnTargetPayload) (*pluginv1.TargetSpec, error) {
	if payload.Name == "" {
		return nil, pluginsdk.ErrNotFound
	}

	config := map[string]*structpb.Value{}
	for k, v := range payload.Args {
		v := hstarlark.FromStarlark(v)

		pv, err := structpb.NewValue(v)
		if err != nil {
			return nil, err
		}

		config[k] = pv
	}

	var transitive *pluginv1.Sandbox
	if !payload.Transitive.Empty() {
		transitiveBuilder := pluginv1.Sandbox_builder{
			Env: make(map[string]*pluginv1.Sandbox_Env),
		}

		for _, tool := range payload.Transitive.Tools {
			ref, err := tref.ParseWithOut(tool)
			if err != nil {
				return nil, err
			}

			transitiveBuilder.Tools = append(transitiveBuilder.Tools, pluginv1.Sandbox_Tool_builder{
				Ref:  ref,
				Hash: htypes.Ptr(true),
			}.Build())
		}

		for name, deps := range payload.Transitive.Deps {
			for _, dep := range deps {
				ref, err := tref.ParseWithOut(dep)
				if err != nil {
					return nil, err
				}

				transitiveBuilder.Deps = append(transitiveBuilder.Deps, pluginv1.Sandbox_Dep_builder{
					Ref:     ref,
					Group:   htypes.Ptr(name),
					Runtime: htypes.Ptr(true),
					Hash:    htypes.Ptr(true),
				}.Build())
			}
		}

		for _, env := range payload.Transitive.PassEnv {
			transitiveBuilder.Env[env] = pluginv1.Sandbox_Env_builder{
				Pass: htypes.Ptr(true),
				Hash: htypes.Ptr(true),
			}.Build()
		}

		for _, env := range payload.Transitive.RuntimePassEnv {
			transitiveBuilder.Env[env] = pluginv1.Sandbox_Env_builder{
				Pass: htypes.Ptr(true),
				Hash: htypes.Ptr(false),
			}.Build()
		}

		transitive = transitiveBuilder.Build()
	}

	spec := pluginv1.TargetSpec_builder{
		Ref:        tref.New(payload.Package, payload.Name, nil),
		Driver:     htypes.Ptr(payload.Driver),
		Config:     config,
		Labels:     payload.Labels,
		Transitive: transitive,
	}.Build()

	return spec, nil
}

func (p *Plugin) getInner(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.TargetSpec, error) {
	var payload OnTargetPayload
	_, err := p.runPkg(ctx, req.GetRef().GetPackage(), Hooks{
		onTarget: func(ctx context.Context, p OnTargetPayload) error {
			if p.Package == req.GetRef().GetPackage() && p.Name == req.GetRef().GetName() {
				payload = p
				return nil // TODO: StopErr
			}

			return nil
		},
	})
	if err != nil {
		return nil, err
	}

	return p.toTargetSpec(ctx, payload)
}
