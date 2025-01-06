package pluginexec

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"

	"connectrpc.com/connect"
	ptylib "github.com/creack/pty"
	"github.com/dlsniper/debugger"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hio"
	"github.com/hephbuild/heph/internal/hpty"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	execv1 "github.com/hephbuild/heph/plugin/pluginexec/gen/heph/plugin/exec/v1"
)

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

	t := &execv1.Target{}
	err := req.Msg.GetTarget().GetDef().UnmarshalTo(t)
	if err != nil {
		return nil, err
	}

	pty := req.Msg.GetTarget().GetPty()
	sandboxfs := hfs.NewOS(req.Msg.GetSandboxPath())
	workfs := hfs.At(sandboxfs, "ws")
	binfs := hfs.At(sandboxfs, "bin")
	cwdfs := hfs.At(workfs, req.Msg.GetTarget().GetRef().GetPackage())

	listArtifacts, err := SetupSandbox(ctx, t, req.Msg.GetInputs(), workfs, binfs, cwdfs)
	if err != nil {
		return nil, err
	}

	env := []string{}

	inputEnv, err := p.inputEnv(ctx, listArtifacts, t.GetDeps())
	if err != nil {
		return nil, err
	}

	env = append(env, inputEnv...)
	env = append(env, fmt.Sprintf("PATH=%v:/usr/sbin:/usr/bin:/sbin:/bin", binfs.Path()))

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
	cmd.Dir = cwdfs.Path()
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
		step.SetError()

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
	deps map[string]*execv1.Target_Deps,
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

		ref := &pluginv1.TargetRefWithOutput{}
		err := input.GetMeta().UnmarshalTo(ref)
		if err != nil {
			return nil, err
		}

		depName, allOutput := getDep(ref)

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
