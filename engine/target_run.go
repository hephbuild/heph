package engine

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
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
}

func (e *TargetRunEngine) Status(s string) {
	if e.Worker != nil {
		e.Worker.Status(s)
	} else {
		log.Info(s)
	}
}

func (e *TargetRunEngine) WarmTargetCache(target *Target) (bool, error) {
	log.Tracef("locking cache %v", target.FQN)
	err := target.cacheLock.Lock()
	if err != nil {
		return false, err
	}

	defer func() {
		log.Tracef("unlocking cache %v", target.FQN)
		err := target.cacheLock.Unlock()
		if err != nil {
			log.Errorf("unlocking cache %v %w", target.FQN, err)
		}
	}()

	if target.ShouldCache {
		e.Status(fmt.Sprintf("Pulling %v cache...", target.FQN))

		dir, err := e.getCache(target)
		if err != nil {
			return false, err
		}

		if dir != nil {
			log.Debugf("Using cache %v", target.FQN)

			target.OutRoot = dir
			err := e.populateActualFiles(target)
			if err != nil {
				return false, err
			}

			return true, err
		}
	}

	return false, nil
}

func (e *Engine) tmpRoot(target *Target) string {
	return filepath.Join(e.HomeDir, "tmp", target.Package.FullName, "__target_"+target.Name)
}

func (e *Engine) lockPath(target *Target, resource string) string {
	return filepath.Join(e.tmpRoot(target), resource+".lock")
}

func (e *TargetRunEngine) Run(target *Target, iocfg sandbox.IOConfig, args ...string) error {
	e.Status(target.FQN)

	log.Tracef("%v locking run", target.FQN)
	err := target.runLock.Lock()
	if err != nil {
		return err
	}

	notifyRan := false
	defer func() {
		log.Tracef("%v unlocking run", target.FQN)
		err := target.runLock.Unlock()
		if err != nil {
			log.Errorf("Failed to unlock %v: %v", target.FQN, err)
		}

		if notifyRan {
			target.ran = true
			close(target.ranCh)
			log.Tracef("Target DONE %v", target.FQN)
			e.Status(fmt.Sprintf("%v done", target.FQN))
		}
	}()

	ctx := e.Context

	if err := ctx.Err(); err != nil {
		return err
	}

	if target.ran {
		return nil
	}

	notifyRan = true

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
			panic(fmt.Sprintf("%v: %v did not run being being used as a tool", target.FQN, tool.Target.FQN))
		}
	}

	for _, dep := range target.Deps.Targets {
		log.Tracef("Dep %v", dep.Target.FQN)

		if !dep.Target.ran {
			panic(fmt.Sprintf("%v: %v did not run being being used as a dep", target.FQN, dep.Target.FQN))
		}
	}

	log.Debugf("Running %v: %v", target.FQN, target.WorkdirRoot.RelRoot)

	bin := map[string]string{}
	for _, t := range target.Tools {
		bin[t.Name] = t.AbsPath()
	}

	for _, t := range target.HostTools {
		bin[t.Name] = t.Path
	}

	log.Tracef("Bin %#v", bin)

	sandboxRoot := e.sandboxRoot(target)
	srcFiles := make([]string, 0)

	var sandboxSpec = sandbox.Spec{}
	if target.Sandbox {
		e.Status(fmt.Sprintf("Creating %v sandbox...", target.FQN))

		src := make([]utils.TarFile, 0)
		srcTar := make([]string, 0)
		for _, dep := range target.Deps.Targets {
			tarFile := e.targetOutputTarFile(target, e.hashInput(target))
			if utils.PathExists(tarFile) && dep.Output == "" {
				srcTar = append(srcTar, tarFile)
			} else {
				files := dep.Target.ActualFilesOut()
				if dep.Output != "" {
					files = dep.Target.NamedActualFilesOut().Name(dep.Output)
				}
				for _, file := range files {
					src = append(src, utils.TarFile{
						From: file.Abs(),
						To:   file.RelRoot(),
					})

					srcFiles = append(srcFiles, file.RelRoot())
				}
			}
		}

		for _, dep := range target.Deps.Files {
			src = append(src, utils.TarFile{
				From: dep.Abs(),
				To:   dep.RelRoot(),
			})
			srcFiles = append(srcFiles, dep.RelRoot())
		}

		var err error
		sandboxSpec, err = sandbox.Make(ctx, sandbox.MakeConfig{
			Root:   sandboxRoot,
			Bin:    bin,
			Src:    src,
			SrcTar: srcTar,
		})
		if err != nil {
			return err
		}
	} else {
		sandboxSpec = sandbox.Spec{
			Root: sandboxRoot,
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

	targetSpecFile, err := os.Create(filepath.Join(sandboxSpec.Root, "target.json"))
	if err != nil {
		return err
	}
	defer targetSpecFile.Close()

	enc := json.NewEncoder(targetSpecFile)
	enc.SetEscapeHTML(false)
	enc.SetIndent("", "    ")
	if err := enc.Encode(target.TargetSpec); err != nil {
		return err
	}

	targetSpecFile.Close()

	if iocfg.Stdout == nil || iocfg.Stderr == nil {
		target.LogFile = filepath.Join(sandboxSpec.Root, "log.txt")

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

	ex, err := os.Executable()
	if err != nil {
		return err
	}

	env := make(map[string]string) // TODO
	env["HEPH"] = ex
	env["TARGET"] = target.FQN
	env["PACKAGE"] = target.Package.FullName
	env["ROOT"] = target.WorkdirRoot.Abs
	env["SANDBOX"] = filepath.Join(e.sandboxRoot(target), "_dir")
	// TODO: figure out causes /bin/bash: argument list too long
	// https://unix.stackexchange.com/a/357859
	//env["SRCS"] = strings.Join(srcFiles, " ")

	for _, t := range target.Tools {
		if t.Target == nil {
			continue
		}

		for k, expr := range t.Target.Provide {
			expr, err := utils.ExprParse(expr)
			if err != nil {
				return err
			}

			switch expr.Function {
			case "outdir":
				outdir, err := e.outdir(t.Target, expr)
				if err != nil {
					return err
				}

				env[strings.ToUpper(t.Target.Name+"_"+k)] = outdir
			default:
				return fmt.Errorf("unhandled function %v", expr.Function)

			}
		}
	}

	for k, v := range target.Env {
		env[k] = v
	}

	cmds := target.Cmds
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

	_, hasPathInEnv := env["PATH"]

	for i, c := range cmds {
		dir := filepath.Join(target.WorkdirRoot.Abs, target.Package.Root.RelRoot)
		if target.RunInCwd {
			if target.ShouldCache {
				return fmt.Errorf("%v cannot run in cwd and cache", target.FQN)
			}

			dir = e.Cwd
		}

		cmd := sandbox.Exec(sandbox.ExecConfig{
			Context:  ctx,
			BinDir:   sandboxSpec.BinDir(),
			Dir:      dir,
			Cmd:      c,
			Env:      env,
			IOConfig: iocfg,
		}, target.Sandbox && !hasPathInEnv)

		log.Tracef("Run %v #%v", target.FQN, i)

		err := cmd.Run()
		if err != nil {
			return fmt.Errorf("exec: %v => %w", c, err)
		}
	}

	e.Status(fmt.Sprintf("Collecting %v output...", target.FQN))

	target.OutRoot = &target.WorkdirRoot
	err = e.populateActualFiles(target)
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
						log.Errorf("store cache %v: %v %v", cache.Name, target.FQN, err)
						return nil
					}

					return nil
				},
			})
		}

		if !e.Config.KeepSandbox && target.Sandbox {
			e.Status(fmt.Sprintf("Clearing %v sandbox...", target.FQN))
			err = deleteDir(target.WorkdirRoot.Abs, false)
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
