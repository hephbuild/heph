package pluginbuildfile

import (
	"context"
	"errors"
	"fmt"
	iofs "io/fs"

	"connectrpc.com/connect"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hstarlark"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
	"go.starlark.net/starlark"
	"go.starlark.net/syntax"
	"google.golang.org/protobuf/types/known/structpb"
)

type Plugin struct {
	repoRoot    hfs.FS
	cacheget    CacheGet
	cacherunpkg CacheRunpkg
}

var _ pluginv1connect.ProviderHandler = (*Plugin)(nil)

const Name = "buildfile"

func New(fs hfs.FS) *Plugin {
	return &Plugin{
		repoRoot: fs,
	}
}

func (p *Plugin) Config(ctx context.Context, req *connect.Request[pluginv1.ProviderConfigRequest]) (*connect.Response[pluginv1.ProviderConfigResponse], error) {
	return connect.NewResponse(&pluginv1.ProviderConfigResponse{
		Name: Name,
	}), nil
}

func (p *Plugin) Probe(ctx context.Context, c *connect.Request[pluginv1.ProbeRequest]) (*connect.Response[pluginv1.ProbeResponse], error) {
	var states []*pluginv1.ProviderState
	_, err := p.runPkg(ctx, hfs.At(p.repoRoot, c.Msg.GetPackage()).Path(), nil, func(ctx context.Context, payload OnProviderStatePayload) error {
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
			Provider: payload.Provider,
			State:    state,
		})

		return nil
	})
	if err != nil {
		return nil, err
	}

	return connect.NewResponse(&pluginv1.ProbeResponse{
		States: states,
	}), nil
}

func (p *Plugin) List(ctx context.Context, req *connect.Request[pluginv1.ListRequest], res *connect.ServerStream[pluginv1.ListResponse]) error {
	err := hfs.Walk(p.repoRoot, func(path string, info iofs.DirEntry, err error) error {
		if err != nil {
			return err
		}

		if !info.IsDir() {
			return nil
		}

		_, err = p.runPkg(ctx, path, func(ctx context.Context, payload OnTargetPayload) error {
			err := res.Send(&pluginv1.ListResponse{
				Ref: &pluginv1.TargetRef{
					Package: payload.Package,
					Name:    payload.Name,
					Driver:  payload.Driver,
				},
			})
			return err
		}, nil)
		if err != nil {
			return err
		}

		return nil
	})
	if err != nil {
		return err
	}

	return nil
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

		return nil, nil
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

func (p *Plugin) Get(ctx context.Context, req *connect.Request[pluginv1.GetRequest]) (*connect.Response[pluginv1.GetResponse], error) {
	spec, err := p.cacheget.Singleflight(ctx, req.Msg.GetRef(), func() (*pluginv1.TargetSpec, error) {
		return p.getInner(ctx, req)
	})
	if err != nil {
		return nil, err
	}

	return connect.NewResponse(&pluginv1.GetResponse{
		Spec: spec,
	}), nil
}

func (p *Plugin) getInner(ctx context.Context, req *connect.Request[pluginv1.GetRequest]) (*pluginv1.TargetSpec, error) {
	var payload OnTargetPayload
	_, err := p.runPkg(ctx, req.Msg.GetRef().GetPackage(), func(ctx context.Context, p OnTargetPayload) error {
		if p.Package == req.Msg.GetRef().GetPackage() && p.Name == req.Msg.GetRef().GetName() {
			payload = p
			return nil // TODO: StopErr
		}

		return nil
	}, nil)
	if err != nil {
		return nil, err
	}

	if payload.Name == "" {
		return nil, connect.NewError(connect.CodeNotFound, errors.New("not found"))
	}

	ref := req.Msg.GetRef()
	ref.Driver = payload.Driver

	config := map[string]*structpb.Value{}
	for k, v := range payload.Args {
		v := hstarlark.FromStarlark(v)

		pv, err := structpb.NewValue(v)
		if err != nil {
			return nil, err
		}

		config[k] = pv
	}

	spec := &pluginv1.TargetSpec{
		Ref:    ref,
		Config: config,
	}

	return spec, nil
}
