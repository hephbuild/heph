package engine

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"time"

	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hdebug"
	"github.com/hephbuild/heph/internal/herrgroup"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hpanic"
	"github.com/hephbuild/heph/internal/hpty"
	"github.com/hephbuild/heph/internal/htar"
	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/lib/hpipe"
	"github.com/hephbuild/heph/lib/pluginsdk"
	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"golang.org/x/sync/semaphore"
	"google.golang.org/protobuf/proto"
)

type ExecuteResultLocks struct {
	*Result
	Locks *CacheLocks
}

func (r *ExecuteResultLocks) Clone() *ExecuteResultLocks {
	return &ExecuteResultLocks{
		Result: r.Result.Clone(),
		Locks:  r.Locks.Clone(),
	}
}

func (r *ExecuteResultLocks) CloneWithoutLocks() *ExecuteResultLocks {
	return &ExecuteResultLocks{
		Result: r.Result.Clone(),
		Locks:  r.Locks,
	}
}

func (r *ExecuteResultLocks) Unlock(ctx context.Context) {
	if r == nil {
		return
	}

	err := r.Locks.Unlock()
	if err != nil {
		hlog.From(ctx).Error(fmt.Sprintf("%v: failed to unlock result locks: %v", tref.Format(r.Def.GetRef()), err))
	}
}

type ExecuteArtifact struct {
	Hashout string
	pluginsdk.Artifact
}

type ExecuteResult struct {
	Def        *LightLinkedTarget
	Hashin     string
	Artifacts  []*ExecuteArtifact
	AfterCache func()
}

func (r ExecuteResult) Sorted() *ExecuteResult {
	slices.SortFunc(r.Artifacts, func(a, b *ExecuteArtifact) int {
		return strings.Compare(a.Hashout, b.Hashout)
	})

	return &r
}

func (r ExecuteResult) Clone() *ExecuteResult {
	return &ExecuteResult{
		Def:        r.Def.Clone(),
		Hashin:     r.Hashin,
		Artifacts:  slices.Clone(r.Artifacts),
		AfterCache: r.AfterCache,
	}
}

type ExecuteResultWithOrigin struct {
	*ExecuteResultLocks
	InputOrigin *pluginv1.TargetDef_InputOrigin
}

type ExecOptions struct {
	Stdin  io.Reader
	Stdout io.Writer
	Stderr io.Writer
}

type ExecuteOptions struct {
	shell       bool
	force       bool
	interactive bool
	metaHashin  string
	getCache    bool
	storeCache  bool
}

type InteractiveExecOptions struct {
	Run func(context.Context, ExecOptions)
	Pty bool
}

var sem = semaphore.NewWeighted(1000 * int64(runtime.GOMAXPROCS(-1)))

func (e *Engine) execute(ctx context.Context, rs *RequestState, def *LightLinkedTarget, results DepsResults, options ExecuteOptions) (*ExecuteResult, error) {
	hashin := options.metaHashin

	ctx, span := tracer.Start(ctx, "Execute")
	defer span.End()

	ctx, cleanLabels := hdebug.SetLabels(ctx, func() []string {
		return []string{
			"where", fmt.Sprintf("Execute %v", tref.Format(def.GetRef())),
		}
	})
	defer cleanLabels()

	driver, ok := e.DriversByName[def.GetDriver()]
	if !ok {
		return nil, fmt.Errorf("driver not found: %v", def.GetDriver())
	}

	if options.shell {
		var err error
		driver, err = e.pickShellDriver(ctx, def)
		if err != nil {
			return nil, err
		}
		def.SetPty(true)
	}

	var targetfolder string
	if def.GetCache() || options.shell {
		targetfolder = e.targetDirName(def.GetRef())
	} else {
		targetfolder = fmt.Sprintf("__%v__%v", e.targetDirName(def.GetRef()), time.Now().UnixNano())
	}

	err := sem.Acquire(ctx, 1)
	if err != nil {
		return nil, err
	}
	defer sem.Release(1)

	step, ctx := hstep.New(ctx, "Running...")
	defer step.Done()

	sandboxfs := hfs.At(e.Sandbox, def.GetRef().GetPackage(), targetfolder)
	workdirfs := hfs.At(sandboxfs, "ws") // TODO: remove the ws from here
	cwdfs := hfs.At(workdirfs, def.GetRef().GetPackage())

	err = sandboxfs.RemoveAll()
	if err != nil {
		return nil, err
	}

	err = sandboxfs.MkdirAll(os.ModePerm)
	if err != nil {
		return nil, err
	}

	execWrapper := func(ctx context.Context, args InteractiveExecOptions) error {
		args.Run(ctx, ExecOptions{})
		return nil
	}

	if options.interactive {
		execWrapper = rs.InteractiveExec
		if execWrapper == nil {
			return nil, errors.New("interactive mode requires interactiveExec")
		}
	}

	var inputLen int
	for _, result := range results {
		inputLen += len(result.Artifacts)
	}

	inputs := make([]*pluginv1.RunRequest_Input, 0, inputLen)
	inputsSdk := make([]*pluginsdk.ArtifactWithOrigin, 0, inputLen)
	for _, result := range results {
		for _, artifact := range result.Artifacts {
			partifact := pluginv1.RunRequest_Input_Artifact_builder{
				Group: htypes.Ptr(artifact.GetGroup()),
				Name:  htypes.Ptr(artifact.GetName()),
				Type:  htypes.Ptr(artifact.GetType()),
				Id:    htypes.Ptr("TODO"), // to be implemented along with the pluginsdkconnect.ProtoArtifact
			}.Build()

			inputs = append(inputs, pluginv1.RunRequest_Input_builder{
				Artifact: partifact,
				Origin:   result.InputOrigin,
			}.Build())

			inputsSdk = append(inputsSdk, &pluginsdk.ArtifactWithOrigin{
				Artifact: artifact.Artifact,
				Origin:   result.InputOrigin,
			})
		}
	}

	var runRes *pluginv1.RunResponse
	var runErr error
	err = execWrapper(ctx, InteractiveExecOptions{
		Run: func(ctx context.Context, options ExecOptions) {
			pctx, cancel := context.WithCancel(context.WithoutCancel(ctx))
			defer cancel()

			pipes, pipesWait, err := e.pipes(pctx, rs, driver, options)
			if err != nil {
				runErr = err
				return
			}
			defer func() {
				if err := pipesWait(); err != nil {
					hlog.From(ctx).Error(fmt.Sprintf("pipe wait: %v", err))
				}
			}()
			defer func() {
				select {
				case <-pctx.Done():
				case <-time.After(200 * time.Millisecond):
					cancel()
				}
			}()

			runRes, runErr = hpanic.RecoverV(func() (*pluginv1.RunResponse, error) {
				return driver.Run(ctx, &pluginsdk.RunRequest{
					RunRequest: pluginv1.RunRequest_builder{
						RequestId:    htypes.Ptr(rs.ID),
						Target:       def.TargetDef.TargetDef,
						SandboxPath:  htypes.Ptr(sandboxfs.Path()),
						TreeRootPath: htypes.Ptr(e.Root.Path()),
						Inputs:       inputs,
						Pipes:        pipes,
						Hashin:       htypes.Ptr(hashin),
					}.Build(),
					Inputs: inputsSdk,
				})
			})
		},
		Pty: def.GetPty(),
	})
	err = errors.Join(err, runErr)
	if err != nil {
		if errors.Is(err, pluginsdk.ErrNotImplemented) {
			return nil, fmt.Errorf("run: %v: %w", def.GetDriver(), err)
		}

		return nil, fmt.Errorf("run: %w", err)
	}

	hashin2, err := e.hashin(ctx, def, results)
	if err != nil {
		return nil, fmt.Errorf("hashin2: %w", err)
	}

	if hashin != hashin2 {
		return nil, errors.New("modified during execution")
	}

	if options.shell {
		return &ExecuteResult{
			Def:    def,
			Hashin: hashin,
			AfterCache: func() {
				err := sandboxfs.RemoveAll()
				if err != nil {
					hlog.From(ctx).Error(fmt.Sprintf("failed to remove sandboxfs: %v", err))
				}
			},
		}, nil
	}

	execArtifacts := make([]*ExecuteArtifact, 0, len(def.OutputNames()))

	for _, output := range def.GetOutputs() {
		shouldCollect := false
		for _, path := range output.GetPaths() {
			if path.GetCollect() {
				shouldCollect = true
				break
			}
		}

		if !shouldCollect {
			continue
		}

		tarname := output.GetGroup() + ".tar"
		tarf, err := hfs.Create(sandboxfs.At("collect", tarname))
		if err != nil {
			return nil, err
		}
		defer tarf.Close()

		tar := htar.NewPacker(tarf)
		defer tar.Close()

		for _, path := range output.GetPaths() {
			if !path.GetCollect() {
				continue
			}

			var globPath string
			switch path.WhichContent() {
			case pluginv1.TargetDef_Path_FilePath_case:
				globPath = path.GetFilePath()
			case pluginv1.TargetDef_Path_DirPath_case:
				globPath = path.GetDirPath()
			case pluginv1.TargetDef_Path_Glob_case:
				globPath = path.GetGlob()
			default:
				return nil, fmt.Errorf("unknown path type: %v", path.WhichContent())
			}

			err := hfs.Glob(ctx, cwdfs, globPath, nil, func(entry hfs.GlobEntry) error {
				f, err := hfs.Open(entry.Node)
				if err != nil {
					return err
				}
				defer f.Close()

				err = tar.WriteFile(f, filepath.Join(tref.ToOSPath(def.GetRef().GetPackage()), entry.RelPath))
				if err != nil {
					return err
				}

				return nil
			})
			if err != nil {
				return nil, fmt.Errorf("collect: %v: %w", path, err)
			}
		}

		err = tar.Close()
		if err != nil {
			return nil, err
		}

		err = tarf.Close()
		if err != nil {
			return nil, err
		}

		execArtifact := pluginsdk.ProtoArtifact{
			Artifact: pluginv1.Artifact_builder{
				Group:   htypes.Ptr(output.GetGroup()),
				Name:    htypes.Ptr(tarname),
				Type:    htypes.Ptr(pluginv1.Artifact_TYPE_OUTPUT),
				TarPath: proto.String(tarf.Name()),
			}.Build(),
		}

		hashout, err := e.hashout(ctx, def.GetRef(), execArtifact)
		if err != nil {
			return nil, err
		}

		execArtifacts = append(execArtifacts, &ExecuteArtifact{
			Hashout:  hashout,
			Artifact: execArtifact,
		})
	}

	{
		shouldCollect := false
		for _, path := range def.GetSupportFiles() {
			if path.GetCollect() {
				shouldCollect = true
				break
			}
		}

		if shouldCollect {
			tarname := "support.tar"
			tarf, err := hfs.Create(sandboxfs.At("collect", tarname))
			if err != nil {
				return nil, err
			}
			defer tarf.Close()

			tar := htar.NewPacker(tarf)
			defer tar.Close()

			for _, path := range def.GetSupportFiles() {
				if !path.GetCollect() {
					continue
				}

				var globPath string
				switch path.WhichContent() {
				case pluginv1.TargetDef_Path_FilePath_case:
					globPath = path.GetFilePath()
				case pluginv1.TargetDef_Path_DirPath_case:
					globPath = path.GetDirPath()
				case pluginv1.TargetDef_Path_Glob_case:
					globPath = path.GetGlob()
				default:
					return nil, fmt.Errorf("unknown support file path type: %v", path.WhichContent())
				}

				err := hfs.Glob(ctx, cwdfs, globPath, nil, func(entry hfs.GlobEntry) error {
					f, err := hfs.Open(entry.Node)
					if err != nil {
						return fmt.Errorf("open: %w", err)
					}
					defer f.Close()

					return tar.WriteFile(f, filepath.Join(tref.ToOSPath(def.GetRef().GetPackage()), entry.RelPath))
				})
				if err != nil {
					return nil, fmt.Errorf("collect support file %v: %w", globPath, err)
				}
			}

			err = tar.Close()
			if err != nil {
				return nil, err
			}

			execArtifact := pluginsdk.ProtoArtifact{
				Artifact: pluginv1.Artifact_builder{
					Name:    htypes.Ptr(tarname),
					Type:    htypes.Ptr(pluginv1.Artifact_TYPE_SUPPORT_FILE),
					TarPath: proto.String(tarf.Name()),
				}.Build(),
			}

			hashout, err := e.hashout(ctx, def.GetRef(), execArtifact)
			if err != nil {
				return nil, err
			}

			execArtifacts = append(execArtifacts, &ExecuteArtifact{
				Hashout:  hashout,
				Artifact: execArtifact,
			})
		}
	}

	for _, partifact := range runRes.GetArtifacts() {
		if partifact.GetType() != pluginv1.Artifact_TYPE_OUTPUT {
			continue
		}

		artifact := pluginsdk.ProtoArtifact{
			Artifact: partifact,
		}

		hashout, err := e.hashout(ctx, def.GetRef(), artifact)
		if err != nil {
			return nil, fmt.Errorf("hashout: %w", err)
		}

		execArtifacts = append(execArtifacts, &ExecuteArtifact{
			Hashout:  hashout,
			Artifact: artifact,
		})
	}

	return ExecuteResult{
		Hashin:    hashin,
		Def:       def,
		Artifacts: execArtifacts,
		AfterCache: func() {
			err := sandboxfs.RemoveAll()
			if err != nil {
				hlog.From(ctx).Error(fmt.Sprintf("failed to remove sandboxfs: %v", err))
			}
		},
	}.Sorted(), nil
}

func (e *Engine) pickShellDriver(ctx context.Context, def *LightLinkedTarget) (pluginsdk.Driver, error) {
	var errs error
	for _, shellDriver := range []string{def.GetDriver() + "@shell"} {
		driver, ok := e.DriversByName[shellDriver]
		if ok {
			return driver, nil
		} else {
			errs = errors.Join(fmt.Errorf("shell driver not found: %v", shellDriver))
		}
	}

	if errs == nil {
		errs = errors.New("no shell driver found")
	}

	return nil, errs
}

func (e *Engine) pipes(ctx context.Context, rs *RequestState, driver pluginsdk.Driver, options ExecOptions) ([]string, func() error, error) {
	pipes := []string{"", "", "", "", ""}
	var eg herrgroup.Group

	var cancels []func()
	var stdinErrCh chan error

	wait := func() error {
		for _, cancel := range cancels {
			cancel()
		}

		err := eg.Wait()

		// this is complicated...
		// if the stdin connected was stdin, and we exec a command, stdin never closes, but the Read interface
		// doesnt have a way to stop reading based on context cancellation, so this just hangs until there is a write into stdin,
		// which makes Read return only to realise that the writer is gone and error out with io: read/write on closed pipe
		// TODO: explore https://github.com/muesli/cancelreader
		if false && stdinErrCh != nil {
			err = errors.Join(err, <-stdinErrCh)
		}

		if errors.Is(err, context.Canceled) {
			return nil
		}

		return err
	}

	driverHandle := e.DriversHandle[driver]

	if options.Stdin != nil {
		stdinErrCh = make(chan error)

		res, err := driver.Pipe(ctx, pluginv1.PipeRequest_builder{
			RequestId: htypes.Ptr(rs.ID),
		}.Build())
		if err != nil && !errors.Is(err, pluginsdk.ErrNotImplemented) {
			return nil, wait, err
		}

		if res != nil && res.GetId() != "" {
			pipes[0] = res.GetId()

			ctx, cancel := context.WithCancel(ctx)
			cancels = append(cancels, cancel)

			go func() {
				defer cancel()

				w, err := hpipe.Writer(ctx, driverHandle.HTTPClientWithOtel(), driverHandle.GetBaseURL(), res.GetPath())
				if err != nil {
					stdinErrCh <- err
					return
				}
				defer w.Close()

				_, err = io.Copy(w, options.Stdin)

				stdinErrCh <- err
			}()
		}
	}

	if options.Stdout != nil {
		res, err := driver.Pipe(ctx, pluginv1.PipeRequest_builder{
			RequestId: htypes.Ptr(rs.ID),
		}.Build())
		if err != nil && !errors.Is(err, pluginsdk.ErrNotImplemented) {
			return nil, wait, err
		}

		if res != nil && res.GetId() != "" {
			pipes[1] = res.GetId()

			eg.Go(func() error {
				r, err := hpipe.Reader(ctx, driverHandle.HTTPClientWithOtel(), driverHandle.GetBaseURL(), res.GetPath())
				if err != nil {
					return err
				}

				_, err = io.Copy(options.Stdout, r)

				return err
			})
		}
	}

	if options.Stderr != nil {
		res, err := driver.Pipe(ctx, pluginv1.PipeRequest_builder{
			RequestId: htypes.Ptr(rs.ID),
		}.Build())
		if err != nil && !errors.Is(err, pluginsdk.ErrNotImplemented) {
			return nil, wait, err
		}

		if res != nil && res.GetId() != "" {
			pipes[2] = res.GetId()

			eg.Go(func() error {
				r, err := hpipe.Reader(ctx, driverHandle.HTTPClientWithOtel(), driverHandle.GetBaseURL(), res.GetPath())
				if err != nil {
					return err
				}

				_, err = io.Copy(options.Stderr, r)

				return err
			})
		}
	}

	if stdin, ok := options.Stdin.(*os.File); ok {
		res, err := driver.Pipe(ctx, pluginv1.PipeRequest_builder{
			RequestId: htypes.Ptr(rs.ID),
		}.Build())
		if err != nil && !errors.Is(err, pluginsdk.ErrNotImplemented) {
			return nil, wait, err
		}
		if res != nil && res.GetId() != "" {
			pipes[3] = res.GetId()

			ch, clean := hpty.WinSizeChan(ctx, stdin)
			cancels = append(cancels, clean)

			go func() {
				w, err := hpipe.Writer(ctx, driverHandle.HTTPClientWithOtel(), driverHandle.GetBaseURL(), res.GetPath())
				if err != nil {
					hlog.From(ctx).Error(fmt.Sprintf("failed to get pipe: %v", err))
					return
				}
				defer w.Close()

				for size := range ch {
					b, err := json.Marshal(size)
					if err != nil {
						hlog.From(ctx).Error(fmt.Sprintf("failed to marshal size: %v", err))
						continue
					}

					_, _ = w.Write(b)
					_, _ = w.Write([]byte("\n"))
				}
			}()
		}
	}

	return pipes, wait, nil
}
