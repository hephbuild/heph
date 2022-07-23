package engine

import (
	"context"
	"errors"
	"fmt"
	"github.com/gofrs/flock"
	log "github.com/sirupsen/logrus"
	"heph/sandbox"
	"heph/utils"
	"heph/worker"
	"os"
	"path/filepath"
	"strings"
)

type TargetRunEngine struct {
	*Engine
	Pool    *worker.Pool
	Worker  *worker.Worker
	Context context.Context

	fileLock *flock.Flock
}

func (e *TargetRunEngine) Status(s string) {
	if e.Worker != nil {
		e.Worker.Status(s)
	} else {
		log.Info(s)
	}
}

func (e *TargetRunEngine) WarmTargetCache(target *Target) (bool, error) {
	if target.ShouldCache {
		e.Status(fmt.Sprintf("Pulling %v cache...", target.FQN))

		dir, err := e.getCache(target)
		if err != nil {
			return false, err
		}

		if dir != nil {
			log.Debugf("Using cache %v", target.FQN)

			target.OutRoot = dir
			err := e.populateActualFilesOut(target)
			if err != nil {
				return false, err
			}

			return true, err
		}
	}

	return false, nil
}

func (e *Engine) lockPath(target *Target) string {
	return filepath.Join(e.sandboxRoot(target), "target.lock")
}

func (e *TargetRunEngine) lock(target *Target) error {
	p := e.lockPath(target)

	if dir := filepath.Dir(p); dir != "." {
		err := os.MkdirAll(dir, os.ModePerm)
		if err != nil {
			return err
		}
	}
	e.fileLock = flock.New(p)

	log.Tracef("%v locking...", target.FQN)

	err := e.fileLock.Lock()
	if err != nil {
		return fmt.Errorf("lock: %v", err)
	}

	return nil
}

func (e *TargetRunEngine) unlock(target *Target) error {
	log.Tracef("%v unlocking...", target.FQN)

	err := e.fileLock.Unlock()
	if err != nil {
		return fmt.Errorf("unlock: %v", err)
	}

	err = os.RemoveAll(e.lockPath(target))
	if err != nil {
		return fmt.Errorf("unlock: rm: %v", err)
	}

	return nil
}

func (e *TargetRunEngine) Run(target *Target, iocfg sandbox.IOConfig, args ...string) error {
	log.Tracef("Target %v", target.FQN)

	err := e.lock(target)
	if err != nil {
		return err
	}

	defer func() {
		err := e.unlock(target)
		if err != nil {
			log.Errorf("Failed to unlock %v: %v", target.FQN, err)
		}

		target.ran = true
		close(target.ranCh)
		log.Tracef("Target DONE %v", target.FQN)
		e.Status(fmt.Sprintf("%v done", target.FQN))
	}()

	ctx := e.Context

	if err := ctx.Err(); err != nil {
		return err
	}

	cached, err := e.WarmTargetCache(target)
	if err != nil {
		return err
	}

	if cached {
		err = e.codegenLink(target)
		if err != nil {
			return err
		}

		return nil
	}

	// Sanity checks
	for _, tool := range target.Tools {
		log.Tracef("Tool %v", tool.Target.FQN)

		if !tool.Target.ran {
			panic(fmt.Sprintf("%v did not run being being used as a tool", tool.Target.FQN))
		}
	}

	for _, dep := range target.Deps.Targets {
		log.Tracef("Dep %v", dep.FQN)

		if !dep.ran {
			panic(fmt.Sprintf("%v did not run being being used as a dep", dep.FQN))
		}
	}

	log.Debugf("Running %v: %v", target.FQN, target.WorkdirRoot.RelRoot)

	bin := map[string]string{}
	for _, t := range target.Tools {
		bin[t.Name] = t.AbsPath()
	}

	log.Tracef("Bin %#v", bin)

	ex, err := os.Executable()
	if err != nil {
		return err
	}

	env := make(map[string]string) // TODO
	env["HEPH"] = ex
	env["TARGET"] = target.FQN
	for k, v := range target.Env {
		env[k] = v
	}

	var sandboxSpec = sandbox.Spec{}
	if target.Sandbox {
		e.Status(fmt.Sprintf("Creating %v sandbox...", target.FQN))

		src := make([]utils.TarFile, 0)
		for _, dep := range target.Deps.Targets {
			for _, file := range dep.ActualFilesOut() {
				src = append(src, utils.TarFile{
					From: file.Abs(),
					To:   file.RelRoot(),
				})
			}
		}

		for _, dep := range target.Deps.Files {
			src = append(src, utils.TarFile{
				From: dep.Abs(),
				To:   dep.RelRoot(),
			})
		}

		var err error
		sandboxSpec, err = sandbox.Make(ctx, sandbox.MakeConfig{
			Root: e.sandboxRoot(target),
			Bin:  bin,
			Src:  src,
		})
		if err != nil {
			return err
		}
	} else {
		sandboxSpec = sandbox.Spec{
			Root: e.sandboxRoot(target),
			Bin:  bin,
		}

		err := sandbox.MakeBin(sandbox.MakeBinConfig{
			Dir: sandboxSpec.BinDir(),
			Bin: bin,
		})
		if err != nil {
			return err
		}
	}

	e.Status(fmt.Sprintf("Running %v...", target.FQN))

	if iocfg.Stdout == nil || iocfg.Stderr == nil {
		target.LogFile = filepath.Join(sandboxSpec.Root, "log.txt")
		err := os.RemoveAll(target.LogFile)
		if err != nil {
			return err
		}

		f, err := os.Create(target.LogFile)
		if err != nil {
			return err
		}
		defer f.Close()

		if iocfg.Stdout == nil {
			iocfg.Stdout = f
		}

		if iocfg.Stderr == nil {
			iocfg.Stderr = f
		}
	}

	cmds := target.Runnable.Cmds
	if len(args) > 0 {
		if len(cmds) > 1 {
			return fmt.Errorf("args are supported only with a single cmd")
		}

		if target.ShouldCache {
			return fmt.Errorf("args are not supported with cache")
		}

		for i := range cmds {
			cmds[i] += " " + strings.Join(args, " ")
		}
	}

	for _, c := range cmds {
		cmd := sandbox.Exec(sandbox.ExecConfig{
			Context:  ctx,
			Spec:     sandboxSpec,
			Dir:      filepath.Join(target.WorkdirRoot.Abs, target.Package.Root.RelRoot),
			Cmd:      c,
			Env:      env,
			IOConfig: iocfg,
		}, target.Sandbox)

		err := cmd.Run()
		if err != nil {
			return fmt.Errorf("exec: %v => %w", c, err)
		}
	}

	e.Status(fmt.Sprintf("Collecting %v output...", target.FQN))

	target.OutRoot = &target.WorkdirRoot
	err = e.populateActualFilesOut(target)
	if err != nil {
		return fmt.Errorf("popfilesout: %w", err)
	}

	if target.ShouldCache {
		e.Status(fmt.Sprintf("Caching %v output...", target.FQN))

		err := e.storeCache(ctx, target)
		if err != nil {
			return fmt.Errorf("cache: store: %w", err)
		}

		for _, cache := range e.Config.Cache {
			cache := cache

			if !cache.Write {
				continue
			}

			e.Pool.Schedule(&worker.Job{
				ID: fmt.Sprintf("cache %v %v", target.FQN, cache.Name),
				Do: func(w *worker.Worker, ctx context.Context) error {
					w.Status(fmt.Sprintf("Pushing %v cache to %v...", target.FQN, cache.Name))

					err = e.storeVfsCache(cache, target)
					if err != nil {
						return err
					}

					return nil
				},
			})
		}

		if target.Sandbox {
			e.Status(fmt.Sprintf("Clearing %v sandbox...", target.FQN))
			err = deleteDir(target.WorkdirRoot.Abs, true)
			if err != nil {
				return err
			}
		}
	}

	err = e.codegenLink(target)
	if err != nil {
		return err
	}

	return nil
}

func (e *TargetRunEngine) codegenLink(target *Target) error {
	if !target.CodegenLink {
		return nil
	}

	e.Status(fmt.Sprintf("Linking %v output", target.FQN))

	for _, file := range target.OutFilesInOutRoot() {
		target := filepath.Join(e.Root, file.RelRoot())

		info, err := os.Lstat(target)
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}

		if err == nil {
			isLink := info.Mode().Type() == os.ModeSymlink

			if !isLink {
				log.Warnf("linking codegen: %v already exists", target)
				continue
			}

			err = os.Remove(target)
			if err != nil {
				return err
			}
		}

		err = os.Symlink(file.Abs(), target)
		if err != nil {
			return err
		}
	}

	return nil
}
