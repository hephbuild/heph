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
	"runtime"
	"slices"
	"strings"
	"syscall"
	"time"

	"github.com/hephbuild/heph/internal/hdebug"
	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hproto"
	"go.opentelemetry.io/otel"
	"golang.org/x/sync/semaphore"

	ptylib "github.com/creack/pty"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hio"
	"github.com/hephbuild/heph/internal/hpty"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	execv1 "github.com/hephbuild/heph/plugin/pluginexec/gen/heph/plugin/exec/v1"
)

var tracer = otel.Tracer("heph/pluginexec")

var sem = semaphore.NewWeighted(int64(runtime.GOMAXPROCS(-1)))

func (p *Plugin) Run(ctx context.Context, req *pluginv1.RunRequest) (*pluginv1.RunResponse, error) {
	ctx, cleanLabels := hdebug.SetLabels(ctx, func() []string {
		return []string{
			"where", fmt.Sprintf("hephpluginexec %v: %v", p.name, tref.Format(req.GetTarget().GetRef())),
		}
	})
	defer cleanLabels()

	step, ctx := hstep.New(ctx, "Executing...")
	defer step.Done()

	err := sem.Acquire(ctx, 1)
	if err != nil {
		return nil, err
	}
	defer sem.Release(1)

	const pipeStdin = 0
	const pipeStdout = 1
	const pipeStderr = 2
	const pipeTermSize = 3

	for len(req.GetPipes()) < 4 {
		req.Pipes = append(req.Pipes, "")
	}

	t := &execv1.Target{}
	err = req.GetTarget().GetDef().UnmarshalTo(t)
	if err != nil {
		return nil, err
	}

	pty := req.GetTarget().GetPty()
	sandboxfs := hfs.NewOS(req.GetSandboxPath())
	workfs := hfs.At(sandboxfs, "ws")
	binfs := hfs.At(sandboxfs, "bin")
	cwdfs := hfs.At(workfs, req.GetTarget().GetRef().GetPackage())
	outfs := cwdfs
	if t.GetContext() == execv1.Target_Tree {
		cwdfs = hfs.At(hfs.NewOS(req.GetTreeRootPath()), req.GetTarget().GetRef().GetPackage())
	}

	listArtifacts, err := SetupSandbox(ctx, t, req.GetInputs(), workfs, binfs, cwdfs, outfs, t.GetContext() != execv1.Target_Tree)
	if err != nil {
		return nil, fmt.Errorf("setup sandbox: %w", err)
	}

	env := make([]string, 0)

	inputEnv, err := p.inputEnv(ctx, listArtifacts, t)
	if err != nil {
		return nil, err
	}

	env = append(env, inputEnv...)
	env = append(env, fmt.Sprintf("PATH=%v:/usr/sbin:/usr/bin:/sbin:/bin:/opt/homebrew/bin:/usr/local/go/bin", binfs.Path())) // TODO: remove /opt/homebrew/bin

	for key, value := range t.GetEnv() {
		env = append(env, fmt.Sprintf("%v=%v", key, value))
	}
	for key, value := range t.GetRuntimeEnv() {
		env = append(env, fmt.Sprintf("%v=%v", key, value))
	}
	for _, name := range t.GetRuntimePassEnv() {
		env = append(env, fmt.Sprintf("%v=%v", name, os.Getenv(name)))
	}
	for _, name := range t.GetPassEnv() {
		env = append(env, fmt.Sprintf("%v=%v", name, os.Getenv(name)))
	}
	env = append(env, fmt.Sprintf("WORKDIR=%v", workfs.Path()))         // TODO: figure it out
	env = append(env, fmt.Sprintf("ROOTDIR=%v", req.GetTreeRootPath())) // TODO: figure it out

	for _, output := range t.GetOutputs() {
		paths := slices.Clone(output.GetPaths())
		for i, path := range paths {
			paths[i] = outfs.Path(path)
		}
		pathsStr := strings.Join(paths, " ")

		env = append(env, getEnvEntry("OUT", output.GetGroup(), "", pathsStr))
	}

	hlog.From(ctx).Debug(fmt.Sprintf("run: %#v", t.GetRun()))
	args := p.runToExecArgs(req.GetSandboxPath(), t.GetRun(), nil)
	hlog.From(ctx).Debug(fmt.Sprintf("args: %#v", args))

	var stdoutWriters, stderrWriters []io.Writer

	logFile, err := os.Create(filepath.Join(req.GetSandboxPath(), "log.txt"))
	if err != nil {
		return nil, err
	}
	defer logFile.Close()

	stdoutWriters = append(stdoutWriters, logFile)
	stderrWriters = append(stderrWriters, logFile)

	if !hfs.Exists(cwdfs, "") {
		return nil, fmt.Errorf("cwd not found: %v", cwdfs.Path())
	}

	env = append(env, "SHELLOPTS=")

	env, err = FilterLongEnv(env, args)
	if err != nil {
		return nil, err
	}

	cmd := exec.CommandContext(ctx, args[0], args[1:]...) //nolint:gosec
	cmd.Env = env
	cmd.Dir = cwdfs.Path()
	cmd.SysProcAttr = &syscall.SysProcAttr{
		Setsid: true, // this creates a new process group, same as Setpgid
	}
	cmd.WaitDelay = 5 * time.Second // TODO: parameterize this
	cmd.Cancel = func() error {
		p := cmd.Process
		if p == nil {
			return nil
		}

		return p.Signal(os.Interrupt)
	}

	for i, id := range []string{req.GetPipes()[pipeStdout], req.GetPipes()[pipeStderr]} {
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

	if id := req.GetPipes()[pipeStdin]; id != "" {
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
		if id := req.GetPipes()[pipeTermSize]; id != "" {
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

		// err = fmt.Errorf("%v: %w", args, err)

		if cerr := ctx.Err(); cerr != nil {
			err = fmt.Errorf("%w: %w", cerr, err)
		}

		cmderr := err

		if !pty { // TODO: if not interactive
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
			Name: "log.txt",
			Type: pluginv1.Artifact_TYPE_LOG,
			Content: &pluginv1.Artifact_File{File: &pluginv1.Artifact_ContentFile{
				SourcePath: logFile.Name(),
				OutPath:    "log.txt",
			}},
		})
	}

	return &pluginv1.RunResponse{
		Artifacts: artifacts,
	}, nil
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

func Merge(ds ...map[string]*execv1.Target_Deps) map[string]*execv1.Target_Deps {
	nd := map[string]*execv1.Target_Deps{}
	for _, deps := range ds {
		for k, v := range deps {
			if _, ok := nd[k]; !ok {
				nd[k] = &execv1.Target_Deps{}
			}

			nd[k].Targets = append(nd[k].Targets, v.GetTargets()...)
			nd[k].Files = append(nd[k].Files, v.GetFiles()...)
		}
	}

	return nd
}

func (p *Plugin) inputEnv(ctx context.Context, inputs []*pluginv1.ArtifactWithOrigin, t *execv1.Target) ([]string, error) {
	m := map[string][]*pluginv1.ArtifactWithOrigin{}

	for name, deps := range Merge(t.GetDeps(), t.GetRuntimeDeps()) {
		for _, dep := range deps.GetTargets() {
			id := dep.GetId()

			allOutput := dep.Ref.Output == nil

			for artifact := range ArtifactsForId(inputs, id, pluginv1.Artifact_TYPE_OUTPUT_LIST_V1) {
				var outputName string
				if allOutput {
					outputName = artifact.GetArtifact().GetGroup()
				}

				envName := getEnvName("SRC", name, outputName)

				m[envName] = append(m[envName], artifact)
			}
		}
	}

	env := make([]string, 0, len(m))
	var sb strings.Builder
	for name, artifacts := range m {
		seenFiles := map[string]struct{}{}
		sb.Reset()

		slices.SortFunc(artifacts, func(a, b *pluginv1.ArtifactWithOrigin) int {
			if v := strings.Compare(a.GetOrigin().GetId(), b.GetOrigin().GetId()); v != 0 {
				return v
			}

			if v := strings.Compare(a.GetArtifact().GetGroup(), a.GetArtifact().GetGroup()); v != 0 {
				return v
			}

			if v := strings.Compare(a.GetArtifact().GetName(), a.GetArtifact().GetName()); v != 0 {
				return v
			}

			if v := hproto.Compare(a, b); v != 0 {
				return v
			}

			return 0
		})

		for _, input := range artifacts {
			r, err := hartifact.FileReader(ctx, input.GetArtifact())
			if err != nil {
				return nil, err
			}
			defer r.Close()

			sc := bufio.NewScanner(r)

			for sc.Scan() {
				line := sc.Text()

				if _, ok := seenFiles[line]; ok {
					continue
				}
				seenFiles[line] = struct{}{}

				if sb.Len() > 0 {
					sb.WriteString(" ")
				}
				sb.WriteString(line)
			}

			_ = r.Close()
		}

		env = append(env, getEnvEntryWithName(name, sb.String()))
	}

	slices.Sort(env)

	return env, nil
}
