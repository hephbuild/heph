package engine

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"heph/log"
	"heph/sandbox"
	"heph/utils"
	"heph/worker"
	"os"
	"path/filepath"
	"regexp"
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

var envRegex = regexp.MustCompile(`[^A-Za-z0-9_]+`)

func normalizeEnv(k string) string {
	return envRegex.ReplaceAllString(k, "_")
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

	for _, dep := range target.Deps.All().Targets {
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

	namedDeps := map[string][]string{}
	addNamedDep := func(name string, file string) {
		a := namedDeps[name]
		a = append(a, file)
		namedDeps[name] = a
	}

	sandboxRoot := e.sandboxRoot(target)
	var sandboxSpec = sandbox.Spec{}
	if target.Sandbox {
		e.Status(fmt.Sprintf("Creating %v sandbox...", target.FQN))

		src := make([]utils.TarFile, 0)
		srcTar := make([]string, 0)
		for name, deps := range target.Deps.Named() {
			for _, dep := range deps.Targets {
				tarFile := e.targetOutputTarFile(dep.Target, e.hashInput(dep.Target))
				if utils.PathExists(tarFile) && dep.Output == "" {
					srcTar = append(srcTar, tarFile)
					for _, file := range dep.Target.ActualFilesOut() {
						file = file.WithPackagePath(true)
						addNamedDep(name, file.RelRoot())
					}
				} else {
					files := dep.Target.ActualFilesOut()
					if dep.Output != "" {
						files = dep.Target.NamedActualFilesOut().Name(dep.Output)
					}
					for _, file := range files {
						file = file.WithPackagePath(true)

						src = append(src, utils.TarFile{
							From: file.Abs(),
							To:   file.RelRoot(),
						})
						addNamedDep(name, file.RelRoot())
					}
				}
			}

			for _, file := range deps.Files {
				to := file.WithPackagePath(true).RelRoot()
				src = append(src, utils.TarFile{
					From: file.Abs(),
					To:   to,
				})
				addNamedDep(name, to)
			}
		}

		for _, file := range src {
			log.Tracef("src: %v", file.To)
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

	dir := filepath.Join(target.WorkdirRoot.Abs, target.Package.FullName)
	if target.RunInCwd {
		if target.ShouldCache {
			return fmt.Errorf("%v cannot run in cwd and cache", target.FQN)
		}

		dir = e.Cwd
	}

	env := make(map[string]string)
	env["TARGET"] = target.FQN
	env["PACKAGE"] = target.Package.FullName
	env["ROOT"] = target.WorkdirRoot.Abs
	env["SANDBOX"] = filepath.Join(e.sandboxRoot(target), "_dir")
	if target.SrcEnv != FileEnvIgnore {
		for name, paths := range namedDeps {
			k := "SRC_" + strings.ToUpper(name)
			if name == "" {
				k = "SRC"
			}
			spaths := make([]string, 0)
			for _, path := range paths {
				switch target.SrcEnv {
				case FileEnvAbs:
					spaths = append(spaths, filepath.Join(target.WorkdirRoot.Abs, path))
				case FileEnvRelRoot:
					spaths = append(spaths, path)
				case FileEnvRelPkg:
					p := "/root"
					rel, err := filepath.Rel(filepath.Join(p, target.Package.FullName), filepath.Join(p, path))
					if err != nil {
						return err
					}
					rel = strings.TrimPrefix(rel, p)
					rel = strings.TrimPrefix(rel, "/")
					spaths = append(spaths, rel)
				default:
					panic("unhandled src_env: " + target.SrcEnv)
				}
			}
			env[normalizeEnv(k)] = strings.Join(spaths, " ")
		}
	}

	if target.OutEnv != FileEnvIgnore {
		namedOut := map[string][]string{}
		for name, paths := range target.Out.WithRoot(target.WorkdirRoot.Abs).Named() {
			for _, path := range paths {
				if strings.Contains(path.Path, "*") {
					// Skip glob
					continue
				}

				if filepath.Base(path.Path) == "." {
					// Skip dot folder
					continue
				}

				path = path.WithPackagePath(true)

				var pathv string
				switch target.OutEnv {
				case FileEnvAbs:
					pathv = path.Abs()
				case FileEnvRelRoot:
					pathv = path.RelRoot()
				case FileEnvRelPkg:
					p := "/root"
					rel, err := filepath.Rel(filepath.Join(p, target.Package.FullName), filepath.Join(p, path.RelRoot()))
					if err != nil {
						return err
					}
					rel = strings.TrimPrefix(rel, p)
					rel = strings.TrimPrefix(rel, "/")
					pathv = rel
				default:
					panic("unhandled out_env: " + target.OutEnv)
				}

				a := namedOut[name]
				a = append(a, pathv)
				namedOut[name] = a
			}
		}

		for name, paths := range namedOut {
			k := "OUT_" + strings.ToUpper(name)
			if name == "" {
				k = "OUT"
			}

			env[normalizeEnv(k)] = strings.Join(paths, " ")
		}
	}

	for _, t := range target.Tools {
		k := "TOOL_" + strings.ToUpper(t.Name)
		if t.Name == "" {
			k = "TOOL"
		}
		env[normalizeEnv(k)] = t.AbsPath()

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

	_, hasPathInEnv := env["PATH"]

	if len(args) > 0 {
		if target.Executor == ExecutorBash && len(target.Run) > 1 {
			return fmt.Errorf("args are supported only with a single cmd")
		}

		if target.ShouldCache {
			return fmt.Errorf("args are not supported with cache")
		}
	}

	if len(target.Run) > 0 {
		var execArgs []string
		switch target.Executor {
		case ExecutorBash:
			execArgs = sandbox.BashArgs(target.Run)
		case ExecutorExec:
			execArgs, err = sandbox.ExecArgs(target.Run, env)
			if err != nil {
				return err
			}
		}

		cmd := sandbox.Exec(sandbox.ExecConfig{
			Context:  ctx,
			BinDir:   sandboxSpec.BinDir(),
			Dir:      dir,
			Env:      env,
			IOConfig: iocfg,
			ExecArgs: execArgs,
			CmdArgs:  args,
		}, target.Sandbox && !hasPathInEnv)

		err = cmd.Run()
		if err != nil {
			return fmt.Errorf("exec: %v %v => %w", execArgs, args, err)
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
