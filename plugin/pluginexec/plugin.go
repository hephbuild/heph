package pluginexec

import (
	"connectrpc.com/connect"
	"context"
	"errors"
	"fmt"
	"github.com/dlsniper/debugger"
	"github.com/google/uuid"
	"github.com/hephbuild/hephv2/internal/hcore/hstep"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1/pluginv1connect"
	execv1 "github.com/hephbuild/hephv2/plugin/pluginexec/gen/heph/plugin/exec/v1"
	"google.golang.org/protobuf/reflect/protodesc"
	"google.golang.org/protobuf/types/known/anypb"
	"io"
	"maps"
	"net/http"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

type pipe struct {
	exp  time.Time
	r    *io.PipeReader
	w    *io.PipeWriter
	busy atomic.Bool
}

type RunToExecArgsFunc = func(run []string, termargs []string) []string

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
	}), nil
}

func (p *Plugin) Parse(ctx context.Context, req *connect.Request[pluginv1.ParseRequest]) (*connect.Response[pluginv1.ParseResponse], error) {
	targetSpec, err := Decode[Spec](req.Msg.Spec.Config)
	if err != nil {
		return nil, err
	}

	s := &execv1.Target{
		Run:  targetSpec.Run,
		Deps: map[string]*execv1.Target_Dep{},
	}

	for k, out := range targetSpec.Out {
		s.Outputs = append(s.Outputs, &execv1.Target_Output{
			Group: k,
			Paths: out,
		})
	}

	var collectOutputs []*pluginv1.TargetDef_CollectOutput
	for _, output := range s.Outputs {
		collectOutputs = append(collectOutputs, &pluginv1.TargetDef_CollectOutput{
			Group: output.Group,
			Paths: output.Paths,
		})
	}

	var deps []*pluginv1.TargetDef_Dep
	for name, sdeps := range targetSpec.Deps {
		var execDeps execv1.Target_Dep
		for _, dep := range sdeps {
			str := strings.ReplaceAll(dep, "//", "")
			pkg, rest, ok := strings.Cut(str, ":")
			if !ok {
				return nil, fmt.Errorf("invalid dep spec: %s", str)
			}
			var name string
			var output *string

			if sname, soutput, ok := strings.Cut(rest, "|"); ok {
				name = sname
				output = &soutput
			} else {
				name = rest
			}

			deps = append(deps, &pluginv1.TargetDef_Dep{
				Ref: &pluginv1.TargetRefWithOutput{
					Package: pkg,
					Name:    name,
					Output:  output,
				},
			})

			execDeps.Targets = append(execDeps.Targets, &execv1.Target_Dep_TargetRef{
				Package: pkg,
				Name:    name,
				Output:  output,
			})
		}
		s.Deps[name] = &execDeps
	}

	target, err := anypb.New(s)
	if err != nil {
		return nil, err
	}

	return connect.NewResponse(&pluginv1.ParseResponse{
		Target: &pluginv1.TargetDef{
			Ref:            req.Msg.Spec.Ref,
			Def:            target,
			Deps:           deps,
			Outputs:        slices.Collect(maps.Keys(targetSpec.Out)),
			Cache:          true,
			CollectOutputs: collectOutputs,
			Codegen:        nil,
		},
	}), nil
}

func getEnvName(prefix, group, name string) string {
	group = strings.ToUpper(group)
	name = strings.ToUpper(name)

	if group != "" && name != "" {
		return fmt.Sprintf("%v_%v_%v", prefix, group, name)
	} else if group != "" {
		return fmt.Sprintf("%v_%v", prefix, group)
	} else if name != "" {
		return fmt.Sprintf("%v_%v", prefix, name)
	} else {
		return fmt.Sprintf("%v", prefix)
	}
}

func getEnvEntry(prefix, group, name, value string) string {
	return getEnvEntryWithName(getEnvName(prefix, group, name), value)
}
func getEnvEntryWithName(name, value string) string {
	return name + "=" + value
}

func (p *Plugin) inputEnv(ctx context.Context, inputs []*pluginv1.ArtifactWithOrigin, deps map[string]*execv1.Target_Dep) ([]string, error) {
	getDep := func(t *pluginv1.TargetRefWithOutput) (string, bool) {
		for name, dep := range deps {
			for _, target := range dep.Targets {
				if target.Name == t.Name && target.Package == t.Package {
					return name, target.Output == nil
				}
			}
		}

		return "", false
	}

	m := map[string][]*pluginv1.Artifact{}
	for _, input := range inputs {
		if input.Artifact.Type != pluginv1.Artifact_TYPE_OUTPUT_LIST_V1 {
			continue
		}

		depName, allOutput := getDep(input.Dep.Ref)

		var outputName string
		if allOutput {
			outputName = input.Artifact.Group
		}

		envName := getEnvName("SRC", depName, outputName)

		m[envName] = append(m[envName], input.Artifact)
	}

	var env []string
	var sb strings.Builder
	for name, artifacts := range m {
		sb.Reset()

		for _, input := range artifacts {
			b, err := os.ReadFile(strings.ReplaceAll(input.Uri, "file://", ""))
			if err != nil {
				return nil, err
			}

			incomingValue := strings.ReplaceAll(string(b), "\n", " ")

			if len(incomingValue) == 0 {
				continue
			}

			if sb.Len() > 0 {
				sb.WriteString(" ")
			}
			sb.WriteString(incomingValue)
		}

		env = append(env, getEnvEntryWithName(name, sb.String()))
	}

	return env, nil
}

func (p *Plugin) Run(ctx context.Context, req *connect.Request[pluginv1.RunRequest]) (*connect.Response[pluginv1.RunResponse], error) {
	debugger.SetLabels(func() []string {
		return []string{
			fmt.Sprintf("hephv2/pluginexec %v: %v %v", p.name, req.Msg.Target.Ref.Package, req.Msg.Target.Ref.Name), "",
		}
	})

	step, ctx := hstep.New(ctx, "Executing...")
	defer step.Done()

	for len(req.Msg.Pipes) < 3 {
		req.Msg.Pipes = append(req.Msg.Pipes, "")
	}

	var t execv1.Target
	err := req.Msg.Target.Def.UnmarshalTo(&t)
	if err != nil {
		return nil, err
	}

	workdir := filepath.Join(req.Msg.SandboxPath, "ws")

	err = os.MkdirAll(workdir, 0755)
	if err != nil {
		return nil, err
	}

	env := []string{}

	inputEnv, err := p.inputEnv(ctx, req.Msg.Inputs, t.Deps)
	if err != nil {
		return nil, err
	}

	env = append(env, inputEnv...)

	for _, output := range t.Outputs {
		paths := strings.Join(output.Paths, " ") // TODO: make it a path

		env = append(env, getEnvEntry("OUT", output.Group, "", paths))
	}

	var stdoutWriters, stderrWriters []io.Writer

	logFile, err := os.Create(filepath.Join(req.Msg.SandboxPath, "log.txt"))
	if err != nil {
		return nil, err
	}
	defer logFile.Close()

	stdoutWriters = append(stdoutWriters, logFile)
	stderrWriters = append(stderrWriters, logFile)

	for i, id := range req.Msg.Pipes[1:3] {
		if id == "" {
			continue
		}

		pipe, ok := p.getPipe(id)
		if !ok {
			return nil, errors.New("pipe 0 not found")
		}

		defer p.removePipe(id)

		if i == 0 {
			stdoutWriters = append(stdoutWriters, pipe.w)
		} else {
			stderrWriters = append(stderrWriters, pipe.w)
		}
	}

	args := p.runToExecArgs(t.Run, nil)

	cmd := exec.CommandContext(ctx, args[0], args[1:]...)
	cmd.Env = env
	cmd.Dir = workdir

	// TODO: pty
	if id := req.Msg.Pipes[0]; id != "" {
		pipe, ok := p.getPipe(id)
		if !ok {
			return nil, errors.New("pipe 0 not found")
		}
		pw, err := cmd.StdinPipe()
		if err != nil {
			return nil, err
		}

		go func() {
			_, _ = io.Copy(pw, pipe.r)
			_ = pw.Close()
		}()
	}
	cmd.Stdout = io.MultiWriter(stdoutWriters...)
	cmd.Stderr = io.MultiWriter(stderrWriters...)

	// TODO: kill all children

	err = cmd.Run()
	if err != nil {
		cmderr := err

		err = logFile.Close()
		if err != nil {
			return nil, err
		}

		b, _ := os.ReadFile(logFile.Name())

		if len(b) > 0 {
			cmderr = errors.Join(cmderr, errors.New(string(b)))
		}

		return nil, cmderr
	}

	stat, err := logFile.Stat()
	if err != nil {
		return nil, err
	}

	err = logFile.Close()
	if err != nil {
		return nil, err
	}

	var artifacts []*pluginv1.Artifact
	if stat.Size() > 0 {
		artifacts = append(artifacts, &pluginv1.Artifact{
			Name: "log.txt",
			Type: pluginv1.Artifact_TYPE_LOG,
			Uri:  "file://" + logFile.Name(),
		})
	}

	return connect.NewResponse(&pluginv1.RunResponse{
		Artifacts: artifacts,
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
		runToExecArgs: func(run []string, termargs []string) []string {
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
	options = append(options, WithRunToExecArgs(func(run []string, termargs []string) []string {
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

func shArgs(initfile string, so []string) []string {
	base := []string{"sh"}
	if initfile != "" {
		base = []string{"env", "ENV=" + initfile, "sh"}
	}
	return append(base, so...)
}

func NewSh(options ...Option) *Plugin {
	options = append(options, WithRunToExecArgs(func(run []string, termargs []string) []string {
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
