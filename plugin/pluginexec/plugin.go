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

	"github.com/hephbuild/heph/internal/hproto/hashpb"
	"github.com/hephbuild/heph/internal/htypes"

	"github.com/hephbuild/heph/internal/hfs"

	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/lib/pluginsdk"
	"github.com/zeebo/xxh3"

	"github.com/google/uuid"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
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
	path          []string
	pathStr       string

	pipes  map[string]*pipe
	pipesm sync.RWMutex
}

func (p *Plugin) PipesHandler() (string, http.Handler) {
	return PipesHandlerPath + "/", PipesHandler{p}
}

func (p *Plugin) Pipe(ctx context.Context, req *pluginv1.PipeRequest) (*pluginv1.PipeResponse, error) {
	p.pipesm.Lock()
	defer p.pipesm.Unlock()

	id := uuid.New().String()

	r, w := io.Pipe()

	p.pipes[id] = &pipe{exp: time.Now().Add(time.Minute), r: r, w: w}

	return pluginv1.PipeResponse_builder{
		Path: htypes.Ptr(path.Join(PipesHandlerPath, id)),
		Id:   htypes.Ptr(id),
	}.Build(), nil
}

func (p *Plugin) Config(ctx context.Context, c *pluginv1.ConfigRequest) (*pluginv1.ConfigResponse, error) {
	desc := (&execv1.Target{}).ProtoReflect().Descriptor()
	pdesc := protodesc.ToDescriptorProto(desc)

	return pluginv1.ConfigResponse_builder{
		Name:         htypes.Ptr(p.name),
		TargetSchema: pdesc,
	}.Build(), nil
}

func depId(prop string, group string, i int) string {
	return fmt.Sprintf("%q %q %v", prop, group, i)
}

func (p *Plugin) Parse(ctx context.Context, req *pluginv1.ParseRequest) (*pluginv1.ParseResponse, error) {
	var targetSpec Spec
	targetSpec.Cache.Remote = true
	targetSpec.Cache.Local = true

	err := hstructpb.DecodeTo(req.GetSpec().GetConfig(), &targetSpec)
	if err != nil {
		return nil, err
	}

	target := execv1.Target_builder{
		Run:            targetSpec.Run,
		Deps:           map[string]*execv1.Target_Deps{},
		HashDeps:       map[string]*execv1.Target_Deps{},
		RuntimeDeps:    map[string]*execv1.Target_Deps{},
		Env:            targetSpec.Env,
		RuntimeEnv:     targetSpec.RuntimeEnv,
		PassEnv:        targetSpec.PassEnv,
		RuntimePassEnv: targetSpec.RuntimePassEnv,
		Context:        htypes.Ptr(execv1.Target_SoftSandbox),
	}.Build()

	if targetSpec.InTree {
		target.SetContext(execv1.Target_Tree)
	}

	var codegenPaths []string
	for k, out := range targetSpec.Out {
		target.SetOutputs(append(target.GetOutputs(), execv1.Target_Output_builder{
			Group: htypes.Ptr(k),
			Paths: out,
		}.Build()))

		for _, s := range out {
			if hfs.IsGlob(s) {
				base, _ := hfs.GlobSplit(s)
				if !strings.HasSuffix(base, "/") {
					base += "/"
				}

				codegenPaths = append(codegenPaths, base)
			} else {
				codegenPaths = append(codegenPaths, s)
			}
		}
	}

	for _, output := range target.GetOutputs() {
		slices.SortFunc(output.GetPaths(), strings.Compare)
	}

	slices.SortFunc(target.GetOutputs(), func(a, b *execv1.Target_Output) int {
		return strings.Compare(a.GetGroup(), b.GetGroup())
	})

	collectOutputs := make([]*pluginv1.TargetDef_CollectOutput, 0, len(target.GetOutputs()))
	for _, output := range target.GetOutputs() {
		collectOutputs = append(collectOutputs, pluginv1.TargetDef_CollectOutput_builder{
			Group: htypes.Ptr(output.GetGroup()),
			Paths: output.GetPaths(),
		}.Build())
	}

	var inputs []*pluginv1.TargetDef_Input
	for name, deps := range hmaps.Sorted(targetSpec.Deps.Merge(targetSpec.HashDeps, targetSpec.RuntimeDeps)) {
		var execDeps execv1.Target_Deps
		for i, dep := range deps {
			ref, err := tref.ParseWithOut(dep)
			if err != nil {
				return nil, fmt.Errorf("dep[%v][%d]: %q: %w", name, i, dep, err)
			}

			meta, err := anypb.New(ref)
			if err != nil {
				return nil, err
			}

			id := depId("deps", name, i)
			inputs = append(inputs, pluginv1.TargetDef_Input_builder{
				Ref: ref,
				Origin: pluginv1.TargetDef_InputOrigin_builder{
					Meta: meta,
					Id:   htypes.Ptr(id),
				}.Build(),
			}.Build())

			execDeps.SetTargets(append(execDeps.GetTargets(), execv1.Target_InputRef_builder{
				Ref: ref,
				Id:  htypes.Ptr(id),
			}.Build()))
		}
		target.GetDeps()[name] = &execDeps
	}

	for _, tools := range targetSpec.Tools {
		for i, tool := range tools {
			ref, err := tref.ParseWithOut(tool)
			if err != nil {
				return nil, fmt.Errorf("tool[%d]: %q: %w", i, tool, err)
			}

			meta, err := anypb.New(ref)
			if err != nil {
				return nil, err
			}

			id := depId("tools", "", i)
			inputs = append(inputs, pluginv1.TargetDef_Input_builder{
				Ref: ref,
				Origin: pluginv1.TargetDef_InputOrigin_builder{
					Meta: meta,
					Id:   htypes.Ptr(id),
				}.Build(),
			}.Build())
			target.SetTools(append(target.GetTools(), execv1.Target_InputRef_builder{
				Ref: ref,
				Id:  htypes.Ptr(id),
			}.Build()))
		}
	}

	hash := xxh3.New()
	desc := target.ProtoReflect().Descriptor()
	hashpb.Hash(hash, target, map[string]struct{}{
		string(desc.FullName()) + ".runtime_deps":     {},
		string(desc.FullName()) + ".runtime_env":      {},
		string(desc.FullName()) + ".runtime_pass_env": {},
	})

	targetAny, err := anypb.New(target)
	if err != nil {
		return nil, err
	}

	var codegenTree []*pluginv1.TargetDef_CodegenTree
	switch targetSpec.Codegen {
	case "":
		// no codegen
	case "copy":
		for _, p := range codegenPaths {
			codegenTree = append(codegenTree, pluginv1.TargetDef_CodegenTree_builder{
				Mode:  htypes.Ptr(pluginv1.TargetDef_CodegenTree_CODEGEN_MODE_COPY),
				Path:  htypes.Ptr(p),
				IsDir: htypes.Ptr(strings.HasSuffix(p, "/")),
			}.Build())
		}
	case "link":
		for _, p := range codegenPaths {
			codegenTree = append(codegenTree, pluginv1.TargetDef_CodegenTree_builder{
				Mode:  htypes.Ptr(pluginv1.TargetDef_CodegenTree_CODEGEN_MODE_LINK),
				Path:  htypes.Ptr(p),
				IsDir: htypes.Ptr(strings.HasSuffix(p, "/")),
			}.Build())
		}
	default:
		return nil, fmt.Errorf("invalid codegen mode: %s", targetSpec.Codegen)
	}

	return pluginv1.ParseResponse_builder{
		Target: pluginv1.TargetDef_builder{
			Ref:                req.GetSpec().GetRef(),
			Def:                targetAny,
			Inputs:             inputs,
			Outputs:            slices.Collect(maps.Keys(targetSpec.Out)),
			Cache:              htypes.Ptr(targetSpec.Cache.Local),
			DisableRemoteCache: htypes.Ptr(!targetSpec.Cache.Remote),
			CollectOutputs:     collectOutputs,
			CodegenTree:        codegenTree,
			Pty:                htypes.Ptr(targetSpec.Pty),
			Hash:               hash.Sum(nil),
		}.Build(),
	}.Build(), nil
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

func WithDefaultLinuxPath() Option {
	return WithPath([]string{
		"/usr/local/bin",
		"/usr/bin",
		"/bin",
	})
}

func WithPath(path []string) Option {
	return func(plugin *Plugin) {
		plugin.path = path
		plugin.pathStr = strings.Join(path, ":")
	}
}

const NameExec = "exec"

func New(options ...Option) *Plugin {
	p := &Plugin{
		pipes: map[string]*pipe{},
		runToExecArgs: func(sandboxPath string, run []string, termargs []string) []string {
			return append(run, termargs...)
		},
		name: NameExec,
	}
	WithDefaultLinuxPath()(p)

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

const NameBash = "bash"

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
	}), WithName(NameBash))

	return New(options...)
}

const NameBashShell = "bash@shell"

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
	}), WithName(NameBashShell))

	return New(options...)
}

func shArgs(initfile string, so []string) []string {
	base := []string{"sh"}
	if initfile != "" {
		base = []string{"env", "ENV=" + initfile, "sh"}
	}
	return append(base, so...)
}

const NameSh = "sh"

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
	}), WithName(NameSh))
	return New(options...)
}

var _ pluginsdk.Driver = (*Plugin)(nil)
