package pluginbuildfile

import (
	"connectrpc.com/connect"
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/hephv2/hfs"
	"github.com/hephbuild/hephv2/hstarlark"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1/pluginv1connect"
	"go.starlark.net/starlark"
	"go.starlark.net/syntax"
	"golang.org/x/sync/singleflight"
	"google.golang.org/protobuf/types/known/structpb"
	iofs "io/fs"
)

type Plugin struct {
	repoRoot hfs.FS
	sf       singleflight.Group
}

func New(fs hfs.FS) *Plugin {
	return &Plugin{
		repoRoot: fs,
	}
}

type onTarget = func(ctx context.Context, payload OnTargetPayload) error

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
		})
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

func (p *Plugin) runPkg(ctx context.Context, pkg string, onTarget onTarget) (starlark.StringDict, error) {
	res, err, _ := p.sf.Do(pkg, func() (interface{}, error) {
		res, err := p.runPkgInner(ctx, pkg, onTarget)
		if err != nil {
			return nil, err
		}

		return res, nil
	})

	return res.(starlark.StringDict), err
}

func (p *Plugin) runPkgInner(ctx context.Context, pkg string, onTarget onTarget) (starlark.StringDict, error) {
	fs := hfs.At(p.repoRoot, pkg)
	// TODO: parametrize
	f, err := hfs.Open(fs, "BUILD")
	if err != nil {
		return nil, err
	}
	defer f.Close()

	res, err := p.runFile(ctx, pkg, f, onTarget)
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

type BuiltinFunc = func(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error)

func (p *Plugin) builtinTarget(onTarget onTarget) BuiltinFunc {
	return func(thread *starlark.Thread, fn *starlark.Builtin, args starlark.Tuple, kwargs []starlark.Tuple) (starlark.Value, error) {
		ctx := thread.Local(ctxKey).(context.Context)
		pkg := thread.Local(packageKey).(string)

		var fkwargs []starlark.Tuple
		var otherkwargs = map[string]starlark.Value{}
		for _, item := range kwargs {
			name, arg := item[0].(starlark.String), item[1]

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

		err := onTarget(ctx, payload)
		if err != nil {
			return nil, err
		}

		// TODO: addr
		return starlark.String(payload.Name), nil
	}
}

const (
	ctxKey     = "__ctx"
	packageKey = "__pkg"
)

func (p *Plugin) runFile(ctx context.Context, pkg string, file hfs.File, onTarget onTarget) (starlark.StringDict, error) {
	universe := starlark.StringDict{
		"target": starlark.NewBuiltin("target", p.builtinTarget(onTarget)),
	}
	prog, err := p.buildFile(ctx, file, universe)
	if err != nil {
		return nil, err
	}

	thread := &starlark.Thread{
		Print: func(thread *starlark.Thread, msg string) {
			fmt.Println(msg)
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
	_, prog, err := starlark.SourceProgramOptions(&syntax.FileOptions{}, file.Name(), file, universe.Has)
	if err != nil {
		return nil, err
	}

	return prog, nil
}

func (p *Plugin) Get(ctx context.Context, req *connect.Request[pluginv1.GetRequest]) (*connect.Response[pluginv1.GetResponse], error) {
	var payload OnTargetPayload
	_, err := p.runPkg(ctx, req.Msg.Ref.Package, func(ctx context.Context, p OnTargetPayload) error {
		payload = p
		return nil // TODO: StopErr
	})
	if err != nil {
		return nil, err
	}

	if payload.Name == "" {
		return nil, connect.NewError(connect.CodeNotFound, fmt.Errorf("not found"))
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

	return connect.NewResponse(&pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref:    req.Msg.Ref,
			Config: config,
		},
	}), nil
}

var _ pluginv1connect.ProviderHandler = (*Plugin)(nil)
