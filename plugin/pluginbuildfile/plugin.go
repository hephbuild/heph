package pluginbuildfile

import (
	"context"
	"errors"
	"fmt"
	"maps"
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
	_, err := p.runPkg(ctx, hfs.At(p.repoRoot, req.GetPackage()).Path(), nil, func(ctx context.Context, payload OnProviderStatePayload) error {
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
		_, err := p.runPkg(ctx, req.GetPackage(), func(ctx context.Context, payload OnTargetPayload) error {
			spec, err := p.toTargetSpec(ctx, payload)
			if err != nil {
				return err
			}

			err = send(pluginv1.ListResponse_builder{
				Spec: proto.ValueOrDefault(spec),
			}.Build())

			return err
		}, nil)

		return err
	}), nil
}

func (p *Plugin) runPkg(ctx context.Context, pkg string, onTarget onTargetFunc, onProviderState onProviderStateFunc) (starlark.StringDict, error) {
	return p.cacherunpkg.Singleflight(ctx, pkg, onTarget, onProviderState, func(onTarget onTargetFunc, onProviderState onProviderStateFunc) (starlark.StringDict, error) {
		return p.runPkgInner(ctx, pkg, onTarget, onProviderState)
	})
}

func (p *Plugin) runPkgInner(ctx context.Context, pkg string, onTarget onTargetFunc, onProviderState onProviderStateFunc) (starlark.StringDict, error) {
	out := starlark.StringDict{}

	fs := hfs.At(p.repoRoot, pkg)
	for _, pattern := range p.Patterns {
		err := hfs.Glob(ctx, fs, pattern, nil, func(path string, d hfs.DirEntry) error {
			f, err := hfs.Open(fs, path)
			if err != nil {
				return err
			}
			defer f.Close()

			res, err := p.runFile(ctx, pkg, f, onTarget, onProviderState)
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

type OnTargetPayload struct {
	Name       string
	Package    string
	Driver     string
	Labels     []string
	Transitive TransitiveSpec

	Args map[string]starlark.Value
}

type TargetSpec struct {
	Name       string
	Driver     string
	Labels     hstarlark.List[string]
	Transitive TransitiveSpec
}

type TransitiveSpec struct {
	Tools hstarlark.List[string]
}

func (ts *TransitiveSpec) Empty() bool {
	return len(ts.Tools) == 0
}

func (c *TransitiveSpec) Unpack(v starlark.Value) error {
	if _, ok := v.(starlark.NoneType); ok {
		return nil
	}

	d, err := hstarlark.UnpackDistruct(v)
	if err != nil {
		return err
	}

	var cs TransitiveSpec
	err = starlark.UnpackArgs("", nil, d.Items().Tuples(),
		"tools?", &cs.Tools,
	)
	if err != nil {
		return err
	}

	*c = cs

	return nil
}

type onTargetFunc = func(ctx context.Context, payload OnTargetPayload) error

type BuiltinFunc = func(
	thread *starlark.Thread,
	fn *starlark.Builtin,
	args starlark.Tuple,
	kwargs []starlark.Tuple,
) (starlark.Value, error)

func (p *Plugin) glob() BuiltinFunc {
	return func(
		thread *starlark.Thread,
		fn *starlark.Builtin,
		args starlark.Tuple,
		kwargs []starlark.Tuple,
	) (starlark.Value, error) {
		pkg := thread.Local(packageKey).(string) //nolint:errcheck

		var pattern string
		if err := starlark.UnpackArgs(
			"glob", args, kwargs,
			"pattern", &pattern,
		); err != nil {
			return nil, err
		}

		base, pattern := hfs.GlobSplit(pattern)

		return starlark.String(tref.FormatFile(tref.JoinPackage(pkg, base), pattern)), nil
	}
}

func (p *Plugin) builtinTarget(onTarget onTargetFunc) BuiltinFunc {
	return func(
		thread *starlark.Thread,
		fn *starlark.Builtin,
		args starlark.Tuple,
		kwargs []starlark.Tuple,
	) (starlark.Value, error) {
		ctx := thread.Local(ctxKey).(context.Context) //nolint:errcheck
		pkg := thread.Local(packageKey).(string)      //nolint:errcheck

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
			Package:    pkg,
			Driver:     tspec.Driver,
			Labels:     tspec.Labels,
			Transitive: tspec.Transitive,
			Args:       otherkwargs,
		}

		if payload.Name == "" {
			return nil, errors.New("missing name")
		}

		err := onTarget(ctx, payload)
		if err != nil {
			return nil, err
		}

		// TODO: addr
		return starlark.String("//" + pkg + ":" + payload.Name), nil
	}
}

type OnProviderStatePayload struct {
	Package  string
	Provider string
	Args     map[string]starlark.Value
}

type onProviderStateFunc = func(ctx context.Context, payload OnProviderStatePayload) error

func (p *Plugin) builtinProviderState(onState onProviderStateFunc) BuiltinFunc {
	return func(
		thread *starlark.Thread,
		fn *starlark.Builtin,
		args starlark.Tuple,
		kwargs []starlark.Tuple,
	) (starlark.Value, error) {
		ctx := thread.Local(ctxKey).(context.Context) //nolint:errcheck
		pkg := thread.Local(packageKey).(string)      //nolint:errcheck

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
			Package: pkg,
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

		err := onState(ctx, payload)
		if err != nil {
			return nil, err
		}

		return starlark.None, nil
	}
}

const (
	ctxKey     = "__heph_ctx"
	packageKey = "__heph_pkg"
)

func (p *Plugin) runFile(ctx context.Context, pkg string, file hfs.File, onTarget onTargetFunc, onProviderState onProviderStateFunc) (starlark.StringDict, error) {
	if onTarget == nil {
		onTarget = func(ctx context.Context, payload OnTargetPayload) error {
			return nil
		}
	}
	if onProviderState == nil {
		onProviderState = func(ctx context.Context, payload OnProviderStatePayload) error {
			return nil
		}
	}
	universe := starlark.StringDict{
		"target":         starlark.NewBuiltin("target", p.builtinTarget(onTarget)),
		"struct":         starlark.NewBuiltin("struct", starlarkstruct.Make),
		"glob":           starlark.NewBuiltin("target", p.glob()),
		"provider_state": starlark.NewBuiltin("provider_state", p.builtinProviderState(onProviderState)),
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

				res, err := p.runPkg(ctx, rest, nil, nil)
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
	thread.SetLocal(ctxKey, ctx)
	thread.SetLocal(packageKey, pkg)

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
		transitiveBuilder := pluginv1.Sandbox_builder{}

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
	_, err := p.runPkg(ctx, req.GetRef().GetPackage(), func(ctx context.Context, p OnTargetPayload) error {
		if p.Package == req.GetRef().GetPackage() && p.Name == req.GetRef().GetName() {
			payload = p
			return nil // TODO: StopErr
		}

		return nil
	}, nil)
	if err != nil {
		return nil, err
	}

	return p.toTargetSpec(ctx, payload)
}
