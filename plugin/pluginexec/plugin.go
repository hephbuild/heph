package pluginexec

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
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
	"syscall"
	"time"

	"connectrpc.com/connect"
	ptylib "github.com/creack/pty"
	"github.com/dlsniper/debugger"
	"github.com/google/uuid"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hio"
	"github.com/hephbuild/heph/internal/hpty"
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

	s := &execv1.Target{
		Run:            targetSpec.Run,
		Deps:           map[string]*execv1.Target_Dep{},
		HashDeps:       map[string]*execv1.Target_Dep{},
		RuntimeDeps:    map[string]*execv1.Target_Dep{},
		Env:            targetSpec.Env,
		RuntimeEnv:     targetSpec.RuntimeEnv,
		PassEnv:        targetSpec.PassEnv,
		RuntimePassEnv: targetSpec.RuntimePassEnv,
	}

	var allOutputPaths []string
	for k, out := range targetSpec.Out {
		s.Outputs = append(s.Outputs, &execv1.Target_Output{
			Group: k,
			Paths: out,
		})
		allOutputPaths = append(allOutputPaths, out...)
	}

	collectOutputs := make([]*pluginv1.TargetDef_CollectOutput, 0, len(s.GetOutputs()))
	for _, output := range s.GetOutputs() {
		collectOutputs = append(collectOutputs, &pluginv1.TargetDef_CollectOutput{
			Group: output.GetGroup(),
			Paths: output.GetPaths(),
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
			Def:            target,
			Deps:           deps,
			Outputs:        slices.Collect(maps.Keys(targetSpec.Out)),
			Cache:          targetSpec.Cache,
			CollectOutputs: collectOutputs,
			CodegenTree:    codegenTree,
			Pty:            targetSpec.Pty,
		},
	}), nil
}

func getEnvName(prefix, group, name string) string {
	group = strings.ToUpper(group)
	name = strings.ToUpper(name)

	switch {
	case group != "" && name != "":
		return fmt.Sprintf("%v_%v_%v", prefix, group, name)
	case group != "":
		return fmt.Sprintf("%v_%v", prefix, group)
	case name != "":
		return fmt.Sprintf("%v_%v", prefix, name)
	default:
		return prefix
	}
}

func getEnvEntry(prefix, group, name, value string) string {
	return getEnvEntryWithName(getEnvName(prefix, group, name), value)
}
func getEnvEntryWithName(name, value string) string {
	return name + "=" + value
}

func (p *Plugin) inputEnv(
	ctx context.Context,
	inputs []*pluginv1.ArtifactWithOrigin,
	deps map[string]*execv1.Target_Dep,
) ([]string, error) {
	getDep := func(t *pluginv1.TargetRefWithOutput) (string, bool) {
		for name, dep := range deps {
			for _, target := range dep.GetTargets() {
				if target.GetName() == t.GetName() && target.GetPackage() == t.GetPackage() {
					return name, target.Output == nil
				}
			}
		}

		return "", false
	}

	m := map[string][]*pluginv1.Artifact{}
	for _, input := range inputs {
		if input.GetArtifact().GetType() != pluginv1.Artifact_TYPE_OUTPUT_LIST_V1 {
			continue
		}

		depName, allOutput := getDep(input.GetDep().GetRef())

		var outputName string
		if allOutput {
			outputName = input.GetArtifact().GetGroup()
		}

		envName := getEnvName("SRC", depName, outputName)

		m[envName] = append(m[envName], input.GetArtifact())
	}

	env := make([]string, 0, len(m))
	var sb strings.Builder
	for name, artifacts := range m {
		sb.Reset()

		for _, input := range artifacts {
			b, err := os.ReadFile(strings.ReplaceAll(input.GetUri(), "file://", ""))
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
			fmt.Sprintf("heph/pluginexec %v: %v %v", p.name, req.Msg.GetTarget().GetRef().GetPackage(), req.Msg.GetTarget().GetRef().GetName()), "",
		}
	})

	step, ctx := hstep.New(ctx, "Executing...")
	defer step.Done()

	const pipeStdin = 0
	const pipeStdout = 1
	const pipeStderr = 2
	const pipeTermSize = 3

	for len(req.Msg.GetPipes()) < 4 {
		req.Msg.Pipes = append(req.Msg.Pipes, "")
	}

	var t execv1.Target
	err := req.Msg.GetTarget().GetDef().UnmarshalTo(&t)
	if err != nil {
		return nil, err
	}

	pty := req.Msg.GetTarget().GetPty()
	workdir := filepath.Join(req.Msg.GetSandboxPath(), "ws")
	cwd := filepath.Join(workdir, req.Msg.GetTarget().GetRef().GetPackage())

	err = os.MkdirAll(cwd, 0755)
	if err != nil {
		return nil, err
	}

	env := []string{}

	inputEnv, err := p.inputEnv(ctx, req.Msg.GetInputs(), t.GetDeps())
	if err != nil {
		return nil, err
	}

	env = append(env, inputEnv...)

	for _, output := range t.GetOutputs() {
		paths := strings.Join(output.GetPaths(), " ") // TODO: make it a path

		env = append(env, getEnvEntry("OUT", output.GetGroup(), "", paths))
	}

	hlog.From(ctx).Debug(fmt.Sprintf("run: %#v", t.GetRun()))
	args := p.runToExecArgs(req.Msg.GetSandboxPath(), t.GetRun(), nil)
	hlog.From(ctx).Debug(fmt.Sprintf("args: %#v", args))

	var stdoutWriters, stderrWriters []io.Writer

	logFile, err := os.Create(filepath.Join(req.Msg.GetSandboxPath(), "log.txt"))
	if err != nil {
		return nil, err
	}
	defer logFile.Close()

	stdoutWriters = append(stdoutWriters, logFile)
	stderrWriters = append(stderrWriters, logFile)

	cmd := exec.CommandContext(ctx, args[0], args[1:]...) //nolint:gosec
	cmd.Env = env
	cmd.Dir = cwd
	cmd.SysProcAttr = &syscall.SysProcAttr{
		Setsid: true, // this creates a new process group, same as Setpgid
	}

	for i, id := range []string{req.Msg.GetPipes()[pipeStdout], req.Msg.GetPipes()[pipeStderr]} {
		if id == "" {
			continue
		}

		pipe, ok := p.getPipe(id)
		if !ok {
			return nil, errors.New("pipe not found")
		}
		defer p.removePipe(id)

		if i == 0 {
			stdoutWriters = append(stdoutWriters, pipe.w)
		} else {
			stderrWriters = append(stderrWriters, pipe.w)
		}
	}

	cmd.Stdout = hio.MultiWriter(stdoutWriters...)
	cmd.Stderr = hio.MultiWriter(stderrWriters...)

	if id := req.Msg.GetPipes()[pipeStdin]; id != "" {
		pipe, ok := p.getPipe(id)
		if !ok {
			return nil, errors.New("pipe stdin not found")
		}
		defer p.removePipe(id)

		// Stdin must be a file, otherwise exec.Run() will wait for that Reader to close before exiting
		if pty {
			cmd.Stdin = pipe.r
		} else {
			pw, err := cmd.StdinPipe()
			if err != nil {
				return nil, err
			}

			go func() {
				_, _ = io.Copy(pw, pipe.r)
				_ = pw.Close()
			}()
		}
	}

	if pty {
		var sizeChan chan *ptylib.Winsize
		if id := req.Msg.GetPipes()[pipeTermSize]; id != "" {
			sizeChan = make(chan *ptylib.Winsize)
			defer close(sizeChan)

			pipe, ok := p.getPipe(id)
			if !ok {
				return nil, errors.New("pipe term size not found")
			}
			defer p.removePipe(id)

			scanner := bufio.NewScanner(pipe.r)
			go func() {
				for scanner.Scan() {
					var size ptylib.Winsize
					err := json.Unmarshal(scanner.Bytes(), &size)
					if err != nil {
						hlog.From(ctx).Warn(fmt.Sprintf("invalid size: %v", scanner.Text()))
						continue
					}

					sizeChan <- &size
				}
				if err := scanner.Err(); err != nil {
					hlog.From(ctx).Warn(fmt.Sprintf("pipe signal: %v", err))
				}
			}()
		}

		pty, err := hpty.Create(ctx, cmd.Stdin, cmd.Stdout, sizeChan)
		if err != nil {
			return nil, err
		}
		defer pty.Close()

		// if its a file, it will wait for it to close before exiting
		cmd.Stdin = pty
		cmd.Stdout = pty
		cmd.Stderr = pty

		cmd.SysProcAttr.Setctty = true
	}

	err = cmd.Run()
	if err != nil {
		cmderr := err

		if !pty {
			err = logFile.Close()
			if err != nil {
				return nil, err
			}

			b, _ := os.ReadFile(logFile.Name())

			if len(b) > 0 {
				cmderr = errors.Join(cmderr, errors.New(string(b)))
			}
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
			Name:     "log.txt",
			Type:     pluginv1.Artifact_TYPE_LOG,
			Encoding: pluginv1.Artifact_ENCODING_NONE,
			Uri:      "file://" + logFile.Name(),
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
