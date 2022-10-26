package engine

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	log "github.com/sirupsen/logrus"
	"heph/exprs"
	"heph/sandbox"
	"heph/targetspec"
	"heph/utils/fs"
	"heph/worker"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"time"
)

type TargetRunEngine struct {
	*Engine
	Print   func(s string)
	Context context.Context
}

func (e *TargetRunEngine) Status(s string) {
	e.Print(s)
}

func (e *TargetRunEngine) PullTargetMeta(target *Target) (bool, error) {
	return e.warmTargetCache(target, true)
}

func (e *TargetRunEngine) WarmTargetCache(target *Target) (bool, error) {
	return e.warmTargetCache(target, false)
}

func (e *TargetRunEngine) warmTargetCache(target *Target, onlyMeta bool) (bool, error) {
	err := e.linkTarget(target, NewTargets(0))
	if err != nil {
		return false, err
	}

	log.Tracef("locking cache %v", target.FQN)
	err = target.cacheLock.Lock()
	if err != nil {
		return false, err
	}

	defer func() {
		log.Tracef("unlocking cache %v", target.FQN)
		err := target.cacheLock.Unlock()
		if err != nil {
			log.Errorf("unlocking cache %v %v", target.FQN, err)
		}
	}()

	if target.Cache.Enabled {
		ok, dir, err := e.getCache(target, onlyMeta)
		if err != nil {
			return false, err
		}

		if ok {
			log.Debugf("Using cache %v", target.FQN)

			if !onlyMeta {
				if dir == nil {
					panic("dir is nil")
				}

				target.OutRoot = dir
				err := e.populateActualFiles(target)
				if err != nil {
					return false, err
				}
			}

			return true, nil
		}
	}

	return false, nil
}

func (e *Engine) tmpRoot(target *Target) string {
	return filepath.Join(e.HomeDir.Abs(), "tmp", target.Package.FullName, "__target_"+target.Name)
}

func (e *Engine) lockPath(target *Target, resource string) string {
	return filepath.Join(e.tmpRoot(target), resource+".lock")
}

var envRegex = regexp.MustCompile(`[^A-Za-z0-9_]+`)

func normalizeEnv(k string) string {
	return envRegex.ReplaceAllString(k, "_")
}

func (e *TargetRunEngine) createFile(target *Target, name, path string, rec *SrcRecorder, fun func(writer io.Writer) error) (error, func()) {
	f, err := os.Create(filepath.Join(e.tmpRoot(target), name))
	if err != nil {
		return err, func() {}
	}
	defer f.Close()

	cleanup := func() {
		_ = os.Remove(f.Name())
	}

	err = fun(f)
	if err != nil {
		return err, cleanup
	}

	rec.Add(name, f.Name(), path, "")

	return nil, cleanup
}

func (e *TargetRunEngine) Run(rr TargetRunRequest, iocfg sandbox.IOConfig) error {
	target := rr.Target
	e.Status(target.FQN)

	err := e.linkTarget(target, NewTargets(0))
	if err != nil {
		return err
	}

	log.Tracef("%v locking run", target.FQN)
	err = target.runLock.Lock()
	if err != nil {
		return err
	}

	defer func() {
		log.Tracef("%v unlocking run", target.FQN)
		err := target.runLock.Unlock()
		if err != nil {
			log.Errorf("Failed to unlock %v: %v", target.FQN, err)
		}

		log.Tracef("Target DONE %v", target.FQN)
	}()

	target.LogFile = ""
	target.OutRoot = nil
	ctx := e.Context

	if err := ctx.Err(); err != nil {
		return err
	}

	if !rr.Shell && !rr.NoCache {
		start := time.Now()
		defer func() {
			log.Debugf("%v done in %v", target.FQN, time.Since(start))
		}()

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
	}

	// Sanity checks
	for _, tool := range target.Tools.Targets {
		if tool.Target.actualFilesOut == nil {
			panic(fmt.Sprintf("%v: %v did not run being being used as a tool", target.FQN, tool.Target.FQN))
		}
	}

	for _, dep := range target.Deps.All().Targets {
		if dep.Target.actualFilesOut == nil {
			panic(fmt.Sprintf("%v: %v did not run being being used as a dep", target.FQN, dep.Target.FQN))
		}
	}

	log.Debugf("Running %v: %v", target.FQN, target.WorkdirRoot.RelRoot())

	bin := map[string]string{}
	for _, t := range target.Tools.Targets {
		bin[t.Name] = t.AbsPath()
	}

	for _, t := range target.Tools.Hosts {
		bin[t.Name] = t.Path
	}

	log.Tracef("Bin %#v", bin)

	srcRec := &SrcRecorder{}
	sandboxRoot := e.sandboxRoot(target)
	binDir := sandboxRoot.Join("_bin").Abs()

	e.Status(fmt.Sprintf("Creating %v sandbox...", target.FQN))

	for name, deps := range target.Deps.Named() {
		for _, dep := range deps.Targets {
			tarFile := e.targetOutputTarFile(dep.Target, e.hashInput(dep.Target))
			if fs.PathExists(tarFile) && dep.Output == "" {
				srcRec.AddTar(tarFile)
				for _, file := range dep.Target.ActualFilesOut() {
					srcRec.AddNamed(name, file.RelRoot(), dep.Full())
				}
			} else {
				files := dep.Target.ActualFilesOut()
				if dep.Output != "" {
					files = dep.Target.NamedActualFilesOut().Name(dep.Output)
				}
				for _, file := range files {
					srcRec.Add(name, file.Abs(), file.RelRoot(), dep.Full())
				}
			}
		}

		for _, file := range deps.Files {
			srcRec.Add(name, file.Abs(), file.RelRoot(), "")
		}
	}

	err, cleanDeps := e.createFile(target, "heph_deps", ".heph/deps.json", srcRec, func(f io.Writer) error {
		m := map[string]interface{}{}

		for name, deps := range target.Deps.Named() {
			a := make([]string, 0)

			for _, dep := range deps.Targets {
				a = append(a, dep.Full())
			}

			for _, file := range deps.Files {
				a = append(a, file.RelRoot())
			}

			m[name] = a
		}

		return json.NewEncoder(f).Encode(m)
	})
	defer cleanDeps()
	if err != nil {
		return err
	}

	err, cleanOrigin := e.createFile(target, "heph_files_origin", ".heph/files_origin.json", srcRec, func(f io.Writer) error {
		return json.NewEncoder(f).Encode(srcRec.Origin())
	})
	defer cleanOrigin()
	if err != nil {
		return err
	}

	err = sandbox.Make(ctx, sandbox.MakeConfig{
		Dir:    target.SandboxRoot.Abs(),
		BinDir: binDir,
		Bin:    bin,
		Src:    srcRec.Src(),
		SrcTar: srcRec.SrcTar(),
	})
	if err != nil {
		return err
	}

	if iocfg.Stdout == nil || iocfg.Stderr == nil {
		target.LogFile = sandboxRoot.Join("log.txt").Abs()

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

	dir := filepath.Join(target.WorkdirRoot.Abs(), target.Package.FullName)
	if target.RunInCwd {
		if target.Cache.Enabled {
			return fmt.Errorf("%v cannot run in cwd and cache", target.FQN)
		}

		dir = e.Cwd
	}

	env := make(map[string]string)
	env["TARGET"] = target.FQN
	env["PACKAGE"] = target.Package.FullName
	env["ROOT"] = target.WorkdirRoot.Abs()
	env["SANDBOX"] = target.SandboxRoot.Abs()
	if target.SrcEnv != targetspec.FileEnvIgnore {
		for name, paths := range srcRec.Named() {
			k := "SRC_" + strings.ToUpper(name)
			if name == "" {
				k = "SRC"
			}
			spaths := make([]string, 0)
			for _, path := range paths {
				switch target.SrcEnv {
				case targetspec.FileEnvAbs:
					spaths = append(spaths, target.SandboxRoot.Join(path).Abs())
				case targetspec.FileEnvRelRoot:
					spaths = append(spaths, path)
				case targetspec.FileEnvRelPkg:
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

	if target.OutEnv != targetspec.FileEnvIgnore {
		out := target.Out.WithRoot(target.SandboxRoot.Abs()).Named()
		namedOut := map[string][]string{}
		for name, paths := range out {
			for _, path := range paths {
				if strings.Contains(path.RelRoot(), "*") {
					// Skip glob
					continue
				}

				if filepath.Base(path.RelRoot()) == "." {
					// Skip dot folder
					continue
				}

				var pathv string
				switch target.OutEnv {
				case targetspec.FileEnvAbs:
					pathv = path.Abs()
				case targetspec.FileEnvRelRoot:
					pathv = path.RelRoot()
				case targetspec.FileEnvRelPkg:
					fakeRoot := "/root"
					rel, err := filepath.Rel(filepath.Join(fakeRoot, target.Package.FullName), filepath.Join(fakeRoot, path.RelRoot()))
					if err != nil {
						return err
					}
					rel = strings.TrimPrefix(rel, fakeRoot)
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

	for _, tool := range target.Tools.Targets {
		k := "TOOL_" + strings.ToUpper(tool.Name)
		if tool.Name == "" {
			k = "TOOL"
		}
		env[normalizeEnv(k)] = tool.AbsPath()

		if tool.Target == nil {
			continue
		}

		for rk, expr := range tool.Target.RuntimeEnv {
			val, err := exprs.Exec(expr, e.queryFunctions(tool.Target))
			if err != nil {
				return fmt.Errorf("runtime env `%v`: %w", expr, err)
			}

			env[normalizeEnv(strings.ToUpper(tool.Target.Name+"_"+rk))] = val
		}
	}
	for _, tool := range target.Tools.Hosts {
		k := "TOOL_" + strings.ToUpper(tool.Name)
		env[normalizeEnv(k)] = tool.Path
	}

	for k, v := range target.Env {
		env[k] = v
	}

	_, hasPathInEnv := env["PATH"]

	if len(rr.Args) > 0 {
		if target.Cache.Enabled {
			return fmt.Errorf("args are not supported with cache")
		}
	}

	e.Status(fmt.Sprintf("Running %v...", target.FQN))

	if target.IsGroup() {
		// Ignore
	} else if target.IsTextFile() {
		err := os.MkdirAll(dir, os.ModePerm)
		if err != nil {
			return err
		}

		err = os.WriteFile(target.Out.All()[0].WithRoot(target.SandboxRoot.Abs()).Abs(), target.FileContent, os.ModePerm)
		if err != nil {
			return err
		}
	} else if len(target.Run) > 0 {
		var executor sandbox.Executor
		switch target.Executor {
		case targetspec.ExecutorBash:
			executor = sandbox.BashExecutor
		case targetspec.ExecutorExec:
			executor = sandbox.ExecExecutor
		default:
			panic("unhandled executor: " + target.Executor)
		}

		run := make([]string, 0)
		for _, s := range target.Run {
			out, err := exprs.Exec(s, e.queryFunctions(target))
			if err != nil {
				return fmt.Errorf("run `%v`: %w", s, err)
			}

			run = append(run, out)
		}

		if rr.Shell {
			fmt.Println("Shell mode enabled, exit the shell to terminate")
			fmt.Printf("Command:\n%v\n", executor.ShellPrint(run))

			executor = sandbox.BashShellExecutor
		}

		execArgs, err := executor.ExecArgs(sandbox.ExecutorContext{
			Args: run,
			Env:  env,
		})
		if err != nil {
			return err
		}

		cmd := sandbox.Exec(sandbox.ExecConfig{
			Context:  ctx,
			BinDir:   binDir,
			Dir:      dir,
			Env:      env,
			IOConfig: iocfg,
			ExecArgs: execArgs,
			CmdArgs:  rr.Args,
		}, target.Sandbox && !hasPathInEnv)

		err = cmd.Run()
		if err != nil {
			return fmt.Errorf("exec: %w", err)
		}
	}

	if rr.Shell {
		return nil
	}

	e.Status(fmt.Sprintf("Collecting %v output...", target.FQN))

	target.OutRoot = &target.WorkdirRoot
	if target.OutInSandbox {
		target.OutRoot = &target.SandboxRoot
	}

	err = e.populateActualFiles(target)
	if err != nil {
		return fmt.Errorf("popfilesout: %w", err)
	}

	if target.Cache.Enabled {
		e.Status(fmt.Sprintf("Caching %v output...", target.FQN))

		err := e.storeCache(ctx, target)
		if err != nil {
			return fmt.Errorf("cache: store: %w", err)
		}

		if !e.DisableNamedCache {
			for _, cache := range e.Config.Cache {
				cache := cache

				if !cache.Write {
					continue
				}

				if !target.Cache.NamedEnabled(cache.Name) {
					continue
				}

				e.Pool.Schedule(ctx, &worker.Job{
					ID: fmt.Sprintf("cache %v %v", target.FQN, cache.Name),
					Do: func(w *worker.Worker, ctx context.Context) error {
						w.Status(fmt.Sprintf("Pushing %v to %v cache...", target.FQN, cache.Name))

						err = e.storeVfsCache(cache, target)
						if err != nil {
							log.Errorf("store vfs cache %v: %v %v", cache.Name, target.FQN, err)
							return nil
						}

						return nil
					},
				})
			}
		}

		if !e.Config.KeepSandbox {
			e.Status(fmt.Sprintf("Clearing %v sandbox...", target.FQN))
			err = deleteDir(target.SandboxRoot.Abs(), false)
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
		target := e.Root.Join(file.RelRoot()).Abs()

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
