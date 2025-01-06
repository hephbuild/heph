package pluginexec

import (
	"context"
	"fmt"
	"io"
	"maps"
	"net/http"
	"os"
	"path"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/hephbuild/heph/plugin/tref"

	"connectrpc.com/connect"
	"github.com/google/uuid"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
	execv1 "github.com/hephbuild/heph/plugin/pluginexec/gen/heph/plugin/exec/v1"
	"google.golang.org/protobuf/reflect/protodesc"
	"google.golang.org/protobuf/types/known/anypb"
)

type pipe struct {
	exp  time.Time
	r    *io.PipeReader
	w    *io.PipeWriter
	busy atomic.Bool
}

type RunToExecArgsFunc = func(sandboxPath string, run []string, termargs []string) []string

type Plugin struct {
	name          string
	runToExecArgs RunToExecArgsFunc

	pipes  map[string]*pipe
	pipesm sync.RWMutex
}

func (p *Plugin) PipesHandler() (string, http.Handler) {
	return PipesHandlerPath + "/", PipesHandler{p}
}

func (p *Plugin) Pipe(ctx context.Context, req *connect.Request[pluginv1.PipeRequest]) (*connect.Response[pluginv1.PipeResponse], error) {
	p.pipesm.Lock()
	defer p.pipesm.Unlock()

	id := uuid.New().String()

	r, w := io.Pipe()

	p.pipes[id] = &pipe{exp: time.Now().Add(time.Minute), r: r, w: w}

	return connect.NewResponse(&pluginv1.PipeResponse{
		Path: path.Join(PipesHandlerPath, id),
		Id:   id,
	}), nil
}

func (p *Plugin) Config(ctx context.Context, c *connect.Request[pluginv1.ConfigRequest]) (*connect.Response[pluginv1.ConfigResponse], error) {
	desc := protodesc.ToDescriptorProto((&execv1.Target{}).ProtoReflect().Descriptor())

	return connect.NewResponse(&pluginv1.ConfigResponse{
		Name:         p.name,
		TargetSchema: desc,
		IgnoreFromHash: []string{
			"runtime_deps",
			"runtime_env",
			"runtime_pass_env",
		},
	}), nil
}

func (p *Plugin) Parse(ctx context.Context, req *connect.Request[pluginv1.ParseRequest]) (*connect.Response[pluginv1.ParseResponse], error) {
	var targetSpec Spec
	targetSpec.Cache = true
	err := DecodeTo(req.Msg.GetSpec().GetConfig(), &targetSpec)
	if err != nil {
		return nil, err
	}

	target := &execv1.Target{
		Run:            targetSpec.Run,
		Deps:           map[string]*execv1.Target_Deps{},
		HashDeps:       map[string]*execv1.Target_Deps{},
		RuntimeDeps:    map[string]*execv1.Target_Deps{},
		Env:            targetSpec.Env,
		RuntimeEnv:     targetSpec.RuntimeEnv,
		PassEnv:        targetSpec.PassEnv,
		RuntimePassEnv: targetSpec.RuntimePassEnv,
	}

	var allOutputPaths []string
	for k, out := range targetSpec.Out {
		target.Outputs = append(target.Outputs, &execv1.Target_Output{
			Group: k,
			Paths: out,
		})
		allOutputPaths = append(allOutputPaths, out...)
	}

	collectOutputs := make([]*pluginv1.TargetDef_CollectOutput, 0, len(target.GetOutputs()))
	for _, output := range target.GetOutputs() {
		collectOutputs = append(collectOutputs, &pluginv1.TargetDef_CollectOutput{
			Group: output.GetGroup(),
			Paths: output.GetPaths(),
		})
	}

	var deps []*pluginv1.TargetDef_Dep
	for name, sdeps := range targetSpec.Deps.Merge(targetSpec.HashDeps, targetSpec.RuntimeDeps) {
		var execDeps execv1.Target_Deps
		for _, dep := range sdeps {
			ref, err := tref.ParseWithOut(dep)
			if err != nil {
				return nil, err
			}

			meta, err := anypb.New(ref)
			if err != nil {
				return nil, err
			}

			deps = append(deps, &pluginv1.TargetDef_Dep{
				Ref:  ref,
				Meta: meta,
			})

			execDeps.Targets = append(execDeps.Targets, ref)
		}
		target.Deps[name] = &execDeps
	}

	for _, tools := range targetSpec.Tools {
		for _, tool := range tools {
			ref, err := tref.ParseWithOut(tool)
			if err != nil {
				return nil, err
			}

			meta, err := anypb.New(ref)
			if err != nil {
				return nil, err
			}

			deps = append(deps, &pluginv1.TargetDef_Dep{
				Ref:  ref,
				Meta: meta,
			})
			target.Tools = append(target.Tools, ref)
		}
	}

	targetAny, err := anypb.New(target)
	if err != nil {
		return nil, err
	}

	var codegenTree *pluginv1.TargetDef_CodegenTree
	switch targetSpec.Codegen {
	case "":
		// no codegen
	case "copy":
		codegenTree = &pluginv1.TargetDef_CodegenTree{
			Mode:  pluginv1.TargetDef_CodegenTree_CODEGEN_MODE_COPY,
			Paths: allOutputPaths,
		}
	case "link":
		codegenTree = &pluginv1.TargetDef_CodegenTree{
			Mode:  pluginv1.TargetDef_CodegenTree_CODEGEN_MODE_LINK,
			Paths: allOutputPaths,
		}
	default:
		return nil, fmt.Errorf("invalid codegen mode: %s", targetSpec.Codegen)
	}

	return connect.NewResponse(&pluginv1.ParseResponse{
		Target: &pluginv1.TargetDef{
			Ref:            req.Msg.GetSpec().GetRef(),
			Def:            targetAny,
			Deps:           deps,
			Outputs:        slices.Collect(maps.Keys(targetSpec.Out)),
			Cache:          targetSpec.Cache,
			CollectOutputs: collectOutputs,
			CodegenTree:    codegenTree,
			Pty:            targetSpec.Pty,
		},
	}), nil
}

type Option func(*Plugin)

func WithRunToExecArgs(f RunToExecArgsFunc) Option {
	return func(plugin *Plugin) {
		plugin.runToExecArgs = f
	}
}

func WithName(name string) Option {
	return func(plugin *Plugin) {
		plugin.name = name
	}
}

func New(options ...Option) *Plugin {
	p := &Plugin{
		pipes: map[string]*pipe{},
		runToExecArgs: func(sandboxPath string, run []string, termargs []string) []string {
			return append(run, termargs...)
		},
		name: "exec",
	}

	for _, opt := range options {
		opt(p)
	}

	return p
}

func bashArgs(so, lo []string) []string {
	// Bash also interprets a number of multi-character options. These options must appear on the command line
	// before the single-character options to be recognized.
	return append(
		append([]string{"bash", "--noprofile"}, lo...),
		append([]string{"-o", "pipefail"}, so...)...,
	)
}

func NewBash(options ...Option) *Plugin {
	options = append(options, WithRunToExecArgs(func(sandboxPath string, run []string, termargs []string) []string {
		args := bashArgs(
			[]string{ /*"-x",*/ "-u", "-e", "-c", strings.Join(run, "\n")},
			[]string{"--norc"},
		)

		if len(termargs) == 0 {
			return args
		} else {
			// https://unix.stackexchange.com/a/144519
			args = append(args, "bash")
			args = append(args, termargs...)
			return args
		}
	}), WithName("bash"))

	return New(options...)
}

func NewInteractiveBash(options ...Option) *Plugin {
	options = append(options, WithRunToExecArgs(func(sandboxPath string, run []string, termargs []string) []string {
		content, err := RenderInitFile(strings.Join(run, "\n"))
		if err != nil { //nolint:staticcheck
			// TODO: log
		}

		initfilePath := filepath.Join(sandboxPath, "init.sh")

		err = os.WriteFile(initfilePath, []byte(content), 0644) //nolint:gosec
		if err != nil {                                         //nolint:staticcheck
			// TODO: log
		}

		return bashArgs(
			nil,
			[]string{"--rcfile", initfilePath},
		)
	}), WithName("bash@shell"))

	return New(options...)
}

func shArgs(initfile string, so []string) []string {
	base := []string{"sh"}
	if initfile != "" {
		base = []string{"env", "ENV=" + initfile, "sh"}
	}
	return append(base, so...)
}

func NewSh(options ...Option) *Plugin {
	options = append(options, WithRunToExecArgs(func(sandboxPath string, run []string, termargs []string) []string {
		args := shArgs(
			"",
			[]string{ /*"-x",*/ "-u", "-e", "-c", strings.Join(run, "\n")},
		)

		if len(termargs) == 0 {
			return args
		} else {
			// https://unix.stackexchange.com/a/144519
			args = append(args, "sh")
			args = append(args, termargs...)
			return args
		}
	}), WithName("sh"))
	return New(options...)
}

var _ pluginv1connect.DriverHandler = (*Plugin)(nil)
