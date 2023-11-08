package targetrun

import (
	"context"
	"errors"
	"fmt"
	ptylib "github.com/creack/pty"
	"github.com/hephbuild/heph/exprs"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/lcache"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/platform"
	"github.com/hephbuild/heph/sandbox"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/worker"
	"github.com/hephbuild/heph/worker/poolwait"
	"os"
	"path/filepath"
	"strconv"
)

type Request struct {
	Target   *graph.Target
	Args     []string
	Mode     string // run or watch
	Compress bool
	RequestOpts
}

type RequestOpts struct {
	NoCache bool
	Force   bool
	Shell   bool
	// Force preserving cache for uncached targets when --print-out is enabled
	PreserveCache bool
	NoPTY         bool
	PullCache     bool
}

func (e *Runner) Run(ctx context.Context, rr Request, iocfg sandbox.IOConfig) (*Target, error) {
	target := rr.Target

	rtarget, err := e.runPrepare(ctx, rr.Target, rr)
	if err != nil {
		return nil, fmt.Errorf("prepare: %w", err)
	}

	completedCh := make(chan struct{})

	defer func() {
		err := rtarget.SandboxLock.Unlock()
		if err != nil {
			log.Errorf("failed to unlock: %v", err)
		}

		// This needs to happen after the lock is released so that sandbox can be cleared, only once in case of concurrent runs
		close(completedCh)
	}()

	env := rtarget.Env
	binDir := rtarget.BinDir

	dir := filepath.Join(rtarget.WorkdirRoot.Abs(), target.Package.Path)
	if target.RunInCwd {
		if target.Cache.Enabled {
			return nil, fmt.Errorf("%v cannot run in cwd and cache", target.Addr)
		}

		dir = e.Cwd
	}

	status.Emit(ctx, tgt.TargetStatus(target, "Running..."))

	if target.IsTextFile() {
		to := target.Out.All()[0].WithRoot(rtarget.SandboxTreeRoot.Abs()).Abs()

		err := xfs.CreateParentDir(to)
		if err != nil {
			return nil, err
		}

		imode, err := strconv.ParseInt(target.Run[1], 8, 32)
		if err != nil {
			return nil, err
		}
		mode := os.FileMode(imode)

		err = os.WriteFile(to, target.FileContent, os.ModePerm)
		if err != nil {
			return nil, err
		}

		err = os.Chmod(to, mode)
		if err != nil {
			return nil, err
		}
	}

	var logFilePath string
	if rr.Shell || target.IsExecutable() {
		var entrypoint platform.Entrypoint
		switch target.Entrypoint {
		case specs.EntrypointBash:
			entrypoint = platform.BashEntrypoint
		case specs.EntrypointSh:
			entrypoint = platform.ShEntrypoint
		case specs.EntrypointExec:
			entrypoint = platform.ExecEntrypoint
		default:
			panic("unhandled entrypoint: " + target.Entrypoint)
		}

		var run []string
		if target.IsTool() {
			log.Tracef("%v is tool, replacing run", target.Addr)
			run = append(target.Run[1:], e.toolAbsPath(target.ToolTarget()))
		} else {
			run = make([]string, 0, len(target.Run))
			funcs := e.QueryFunctions(target)

			for _, s := range target.Run {
				out, err := exprs.Exec(s, funcs)
				if err != nil {
					return nil, fmt.Errorf("run `%v`: %w", s, err)
				}

				run = append(run, out)
			}
		}

		if rr.Shell {
			for _, e := range []string{"TERM", "COLORTERM"} {
				if _, ok := env[e]; !ok {
					env[e] = os.Getenv(e)
				}
			}
		}

		_, hasPathInEnv := env["PATH"]
		sandbox.AddPathEnv(env, binDir, target.Sandbox && !hasPathInEnv)

		execCtx := ctx
		if target.Timeout > 0 {
			var cancel context.CancelFunc
			execCtx, cancel = context.WithTimeout(ctx, target.Timeout)
			defer cancel()
		}

		execCtx, execSpan := e.Observability.SpanRunExec(execCtx, target)

		obw := e.Observability.LogsWriter(execCtx)

		var logFile *os.File
		if !rr.Shell {
			logFilePath = e.sandboxRoot(target).Join("log.txt").Abs()

			logFile, err = os.Create(logFilePath)
			if err != nil {
				return nil, err
			}
			defer logFile.Close()

			obw = multiWriterNil(obw, logFile)
		}

		if obw != nil {
			var szch chan *ptylib.Winsize
			if iocfg.Stdin != nil {
				ch, cleanwinsize := stdinWinSizeCh()
				szch = ch
				defer cleanwinsize()
			}

			outw := multiWriterNil(iocfg.Stdout, obw)
			errw := multiWriterNil(iocfg.Stderr, obw)

			if !rr.NoPTY && isWriterTerminal(iocfg.Stdout) {
				pty, err, clean := createPty(outw, szch)
				if err != nil {
					return nil, err
				}
				defer clean()

				iocfg.Stdout = pty
			} else {
				iocfg.Stdout = outw
			}

			if !rr.NoPTY && isWriterTerminal(iocfg.Stderr) {
				pty, err, clean := createPty(errw, szch)
				if err != nil {
					return nil, err
				}
				defer clean()

				iocfg.Stderr = pty
			} else {
				iocfg.Stderr = errw
			}
		}

		err = platform.Exec(
			execCtx,
			rtarget.Executor,
			entrypoint,
			e.tmpTargetRoot(target).Abs(),
			platform.ExecOptions{
				WorkDir:  dir,
				BinDir:   binDir,
				HomeDir:  e.Root.Home.Abs(),
				Target:   target.Spec(),
				Env:      env,
				Run:      run,
				TermArgs: rr.Args,
				IOCfg:    iocfg,
			},
			rr.Shell,
		)
		if logFile != nil {
			_ = logFile.Close()
		}
		execSpan.EndError(err)
		if err != nil {
			if rr.Shell {
				log.Debugf("exec: %v", err)
				return nil, nil
			}

			if cerr := ctx.Err(); cerr != nil {
				if !errors.Is(cerr, err) {
					err = fmt.Errorf("%w: %v", cerr, err)
				}
			}

			err := fmt.Errorf("exec: %w", err)

			if iocfg.Stdin == os.Stdin {
				return nil, TargetFailed{
					Target: target,
					Err:    err,
				}
			}

			return nil, TargetFailed{
				Target:  target,
				LogFile: logFilePath,
				Err:     err,
			}
		}
	}

	if rr.Shell {
		return nil, nil
	}

	ltarget, err := e.LocalCache.Target(ctx, rr.Target, lcache.TargetOpts{
		ActualFilesCollector:        lcache.ActualFileCollectorDir{Dir: rtarget.OutRoot.Abs()},
		ActualFilesCollectorOutputs: rr.Target.Out.Names(),
	})
	if err != nil {
		return nil, err
	}

	artifactProducers := e.artifactWithProducers(ltarget, rtarget.OutRoot.Abs(), logFilePath)

	err = e.LocalCache.StoreCache(ctx, target, artifactProducers, rr.Compress)
	if err != nil {
		return nil, fmt.Errorf("cache: store: %w", err)
	}

	if !target.Cache.Enabled && !rr.PreserveCache {
		e.LocalCache.RegisterRemove(target)
	}

	if !e.Config.Engine.KeepSandbox {
		j := e.Pool.Schedule(ctx, &worker.Job{
			Name: fmt.Sprintf("clear sandbox %v", target.Addr),
			// We need to make sure to wait for the lock to be released before proceeding
			Deps: worker.WaitGroupChan(completedCh),
			Do: func(w *worker.Worker, ctx context.Context) error {
				locked, err := rtarget.SandboxLock.TryLock(ctx)
				if err != nil {
					return err
				}

				if !locked {
					return nil
				}

				defer func() {
					err := rtarget.SandboxLock.Unlock()
					if err != nil {
						log.Errorf("Failed to unlock %v: %v", target.Addr, err)
					}
				}()

				status.Emit(ctx, tgt.TargetStatus(target, "Clearing sandbox..."))
				err = xfs.DeleteDir(rtarget.SandboxRoot.Abs(), false)
				if err != nil {
					return fmt.Errorf("clear sandbox: %w", err)
				}

				return nil
			},
		})
		if poolDeps := poolwait.ForegroundWaitGroup(ctx); poolDeps != nil {
			poolDeps.Add(j)
		}
	}

	return rtarget, nil
}

type TargetFailed struct {
	Target  *graph.Target
	LogFile string
	Err     error
}

func (t TargetFailed) Error() string {
	return t.Err.Error()
}

func (t TargetFailed) Unwrap() error {
	return t.Err
}

func (t TargetFailed) Is(target error) bool {
	_, ok := target.(TargetFailed)

	return ok
}

func WrapTargetFailed(err error, target *graph.Target) error {
	if !errors.Is(err, TargetFailed{}) {
		return TargetFailed{Target: target, Err: err}
	}

	return err
}
