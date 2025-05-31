package pluginbuildfile

import (
	"context"
	"errors"
	"fmt"
	"github.com/go-viper/mapstructure/v2"
	engine2 "github.com/hephbuild/heph/lib/engine"
	iofs "io/fs"

	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hstarlark"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"go.starlark.net/starlark"
	"go.starlark.net/syntax"
	"google.golang.org/protobuf/types/known/structpb"
)

type Options struct {
	Patterns []string
}

type Plugin struct {
	Options
	repoRoot    hfs.FS
	cacheget    CacheGet
	cacherunpkg CacheRunpkg
}

var _ engine2.Provider = (*Plugin)(nil)

const Name = "buildfile"

func New(fs hfs.FS, cfg Options) *Plugin {
	return &Plugin{
		Options:  cfg,
		repoRoot: fs,
	}
}

func (p *Plugin) Config(ctx context.Context, req *pluginv1.ProviderConfigRequest) (*pluginv1.ProviderConfigResponse, error) {
	return &pluginv1.ProviderConfigResponse{
		Name: Name,
	}, nil
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

		states = append(states, &pluginv1.ProviderState{
			Package:  payload.Package,
			Provider: payload.Provider,
			State:    state,
		})

		return nil
	})
	if err != nil {
		return nil, err
	}

	return &pluginv1.ProbeResponse{
		States: states,
	}, nil
}

func (p *Plugin) List(ctx context.Context, req *pluginv1.ListRequest) (engine2.HandlerStreamReceive[*pluginv1.ListResponse], error) {
	return engine2.NewChanHandlerStreamFunc(func(send func(*pluginv1.ListResponse) error) error {

		if !req.GetDeep() {
			_, err := p.runPkg(ctx, req.GetPackage(), func(ctx context.Context, payload OnTargetPayload) error {
				spec, err := p.toTargetSpec(ctx, payload)
				if err != nil {
					return err
				}

				err = send(&pluginv1.ListResponse{
					Of: &pluginv1.ListResponse_Spec{
						Spec: spec,
					},
				})

				return err
			}, nil)

			return err
		}

		// TODO: hfs.At(p.repoRoot, req.Package)
		return hfs.Walk(p.repoRoot, func(path string, info iofs.DirEntry, err error) error {
			if err != nil {
				return err
			}

			if !info.IsDir() {
				return nil
			}

			_, err = p.runPkg(ctx, path, func(ctx context.Context, payload OnTargetPayload) error {
				spec, err := p.toTargetSpec(ctx, payload)
				if err != nil {
					return err
				}

				err = send(&pluginv1.ListResponse{
					Of: &pluginv1.ListResponse_Spec{
						Spec: spec,
					},
				})

				return err
			}, nil)
			if err != nil {
				return err
			}

			return nil
		})
	}), nil

}

func (p *Plugin) runPkg(ctx context.Context, pkg string, onTarget onTargetFunc, onProviderState onProviderStateFunc) (starlark.StringDict, error) {
	return p.cacherunpkg.Singleflight(ctx, pkg, onTarget, onProviderState, func(onTarget onTargetFunc, onProviderState onProviderStateFunc) (starlark.StringDict, error) {
		return p.runPkgInner(ctx, pkg, onTarget, onProviderState)
	})
}

func (p *Plugin) runPkgInner(ctx context.Context, pkg string, onTarget onTargetFunc, onProviderState onProviderStateFunc) (starlark.StringDict, error) {
	fs := hfs.At(p.repoRoot, pkg)
	// TODO: parametrize
	f, err := hfs.Open(fs, "BUILD")
	if err != nil {
		if errors.Is(err, hfs.ErrNotExist) {
			return starlark.StringDict{}, nil
		}

		return nil, err
	}
	defer f.Close()

	res, err := p.runFile(ctx, pkg, f, onTarget, onProviderState)
	if err != nil {
		return nil, err
	}

	return res, nil
}

type OnTargetPayload struct {
	Name    string
	Package string
	Driver  string
	Args    map[string]starlark.Value
}

type onTargetFunc = func(ctx context.Context, payload OnTargetPayload) error

type BuiltinFunc = func(
	thread *starlark.Thread,
	fn *starlark.Builtin,
	args starlark.Tuple,
	kwargs []starlark.Tuple,
) (starlark.Value, error)

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
			case "name", "driver":
				fkwargs = append(fkwargs, item)
			default:
				otherkwargs[string(name)] = arg
			}
		}

		payload := OnTargetPayload{
			Package: pkg,
			Args:    otherkwargs,
		}
		if err := starlark.UnpackArgs(
			"target", args, fkwargs,
			"name", &payload.Name,
			"driver?", &payload.Driver,
		); err != nil {
			if payload.Name != "" {
				return nil, fmt.Errorf("%v: %w", payload.Name, err)
			}

			return nil, err
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
	spec, err := p.cacheget.Singleflight(ctx, req.GetRef(), func() (*pluginv1.TargetSpec, error) {
		return p.getInner(ctx, req)
	})
	if err != nil {
		return nil, err
	}

	return &pluginv1.GetResponse{
		Spec: spec,
	}, nil
}

func parseLabels(v any) ([]string, error) {
	var labels []string
	err := mapstructure.Decode(v, &labels)
	if err == nil {
		return labels, nil
	}

	var label string
	err = mapstructure.Decode(v, &label)
	if err == nil {
		return []string{label}, nil
	}

	return nil, fmt.Errorf("expected string or []string, got %T", v)
}

func (p *Plugin) toTargetSpec(ctx context.Context, payload OnTargetPayload) (*pluginv1.TargetSpec, error) {
	if payload.Name == "" {
		return nil, engine2.ErrNotFound
	}

	var labels []string
	config := map[string]*structpb.Value{}
	for k, v := range payload.Args {
		v := hstarlark.FromStarlark(v)

		pv, err := structpb.NewValue(v)
		if err != nil {
			return nil, err
		}

		config[k] = pv

		if k == "labels" {
			labels, err = parseLabels(v)
			if err != nil {
				return nil, fmt.Errorf("invalid labels: %w", err)
			}
		}
	}

	spec := &pluginv1.TargetSpec{
		Ref: &pluginv1.TargetRef{
			Package: payload.Package,
			Name:    payload.Name,
		},
		Driver: payload.Driver,
		Config: config,
		Labels: labels,
	}

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
