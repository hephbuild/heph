package engine

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"go.opentelemetry.io/otel/attribute"
	"heph/engine/htrace"
	"heph/exprs"
	log "heph/hlog"
	"heph/sandbox"
	"heph/targetspec"
	"heph/utils"
	"heph/utils/fs"
	"heph/utils/sets"
	"heph/utils/tar"
	"heph/worker"
	"io"
	fs2 "io/fs"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"time"
)

type TargetRunEngine struct {
	*Engine
	Status func(s worker.Status)
}

func (e *TargetRunEngine) PullTargetMeta(ctx context.Context, target *Target, outputs []string) (bool, error) {
	return e.warmTargetCaches(ctx, target, outputs, true)
}

func (e *TargetRunEngine) WarmTargetCache(ctx context.Context, target *Target, output string) (bool, error) {
	return e.warmTargetOutput(ctx, target, output, false)
}

func (e *TargetRunEngine) WarmAllTargetCache(ctx context.Context, target *Target) (bool, error) {
	return e.warmTargetCaches(ctx, target, target.OutWithSupport.Names(), false)
}

func (e *TargetRunEngine) warmTargetCaches(ctx context.Context, target *Target, outputs []string, onlyMeta bool) (bool, error) {
	outputs = utils.CopyArray(outputs)
	outputs = targetspec.SortOutputsForHashing(outputs)

	cached, err := e.warmTargetInput(ctx, target)
	if err != nil {
		return false, err
	}

	if !cached {
		return false, nil
	}

	for _, output := range outputs {
		cached, err := e.warmTargetOutput(ctx, target, output, onlyMeta)
		if err != nil {
			return false, err
		}

		if !cached {
			return false, nil
		}
	}

	if !onlyMeta {
		err := e.postRunOrWarm(ctx, target, outputs)
		if err != nil {
			return false, err
		}
	}

	return true, nil
}

func (e *TargetRunEngine) warmTargetInput(ctx context.Context, target *Target) (_ bool, rerr error) {
	err := e.LinkTarget(target, NewTargets(0))
	if err != nil {
		return false, err
	}

	if !target.Cache.Enabled {
		return false, nil
	}

	span := e.SpanCachePull(ctx, target, inputHashName, true)
	defer func() {
		span.EndError(rerr)
	}()

	return e.getCache(target, inputHashName, true)
}

func (e *TargetRunEngine) warmTargetOutput(ctx context.Context, target *Target, output string, onlyMeta bool) (_ bool, rerr error) {
	err := e.LinkTarget(target, NewTargets(0))
	if err != nil {
		return false, err
	}

	if !target.Cache.Enabled {
		return false, nil
	}

	if output != inputHashName && !target.OutWithSupport.HasName(output) {
		return false, fmt.Errorf("%v does not output %v", target.FQN, output)
	}

	span := e.SpanCachePull(ctx, target, output, onlyMeta)
	defer func() {
		span.EndError(rerr)
	}()

	ok, err := e.getCache(target, output, onlyMeta)
	if err != nil {
		return false, err
	}

	if !ok {
		return false, nil
	}

	log.Debugf("Using cache %v", target.FQN)

	if onlyMeta {
		return true, nil
	}

	span.SetAttributes(attribute.Bool(htrace.AttrCacheHit, true))
	return true, nil
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

type runPrepare struct {
	Env    map[string]string
	BinDir string
}

func (e *TargetRunEngine) runPrepare(ctx context.Context, target *Target) (_ *runPrepare, rerr error) {
	span := e.SpanRunPrepare(ctx, target)
	defer func() {
		span.EndError(rerr)
	}()

	// Sanity checks
	for _, tool := range target.Tools.Targets {
		if tool.Target.actualOutFiles == nil {
			panic(fmt.Sprintf("%v: %v did not run being being used as a tool", target.FQN, tool.Target.FQN))
		}
	}

	for _, dep := range target.Deps.All().Targets {
		if dep.Target.actualOutFiles == nil {
			panic(fmt.Sprintf("%v: %v did not run being being used as a dep", target.FQN, dep.Target.FQN))
		}
	}

	log.Debugf("Running %v: %v", target.FQN, target.WorkdirRoot.RelRoot())

	bin := map[string]string{}
	for _, t := range target.Tools.Targets {
		bin[t.Name] = t.AbsPath()
	}

	for _, t := range target.Tools.Hosts {
		var err error
		bin[t.Name], err = t.ResolvedPath()
		if err != nil {
			return nil, err
		}
	}

	log.Tracef("Bin %#v", bin)

	// Records all src as files (even tar) to be used for creating SRC vars later
	envSrcRec := &SrcRecorder{}
	// Records src that should be copied, tar & files
	srcRec := &SrcRecorder{Parent: envSrcRec}
	// Records symlinks that should be created
	linkSrcRec := &SrcRecorder{}
	sandboxRoot := e.sandboxRoot(target)
	binDir := sandboxRoot.Join("_bin").Abs()

	e.Status(TargetStatus(target, "Creating sandbox..."))

	restoreSrcRec := &SrcRecorder{}
	if target.RestoreCache {
		latestDir := e.cacheDirForHash(target, "latest").Abs()

		if fs.PathExists(latestDir) {
			done := utils.TraceTiming("Restoring cache")

			for _, name := range target.OutWithSupport.Names() {
				p := e.targetOutputTarFileForHash(target, "latest", name)
				if !fs.PathExists(p) {
					log.Errorf("restore cache: out %v|%v: tar does not exist", target.FQN, name)
					continue
				}
				restoreSrcRec.AddTar(p)
			}

			done()
		}
	}

	var length int
	for _, deps := range target.Deps.Named() {
		var deplength int
		for _, dep := range deps.Targets {
			deplength += len(dep.Target.ActualOutFiles().Name(dep.Output))
		}
		deplength += len(deps.Files)

		length += deplength
	}

	traceFilesList := utils.TraceTiming("Building sandbox files list")

	srcRecNameToDepName := make(map[string]string, length)
	for name, deps := range target.Deps.Named() {
		for _, dep := range deps.Targets {
			dept := dep.Target

			if len(dept.ActualOutFiles().All()) == 0 {
				continue
			}

			if dep.Mode == targetspec.TargetSpecDepModeLink {
				for _, file := range dept.ActualOutFiles().Name(dep.Output) {
					linkSrcRec.Add("", file.Abs(), file.RelRoot(), "")
				}
			} else {
				tarFile := e.targetOutputTarFile(dept, dep.Output)
				srcRec.AddTar(tarFile)
			}

			srcName := name
			if dep.SpecOutput == "" && dep.Output != "" {
				if srcName != "" {
					srcName += "_"
				}
				srcName += dep.Output
			}

			for _, file := range dept.ActualOutFiles().Name(dep.Output) {
				srcRecNameToDepName[srcName] = name
				envSrcRec.Add(srcName, "", file.RelRoot(), dep.Full())
			}
		}

		for _, file := range deps.Files {
			srcRecNameToDepName[name] = name
			srcRec.Add(name, file.Abs(), file.RelRoot(), "")
		}
	}

	traceFilesList()

	err, cleanOrigin := e.createFile(target, "heph_files_origin", ".heph/files_origin.json", srcRec, func(f io.Writer) error {
		return json.NewEncoder(f).Encode(envSrcRec.Origin())
	})
	defer cleanOrigin()
	if err != nil {
		return nil, err
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
		return nil, err
	}

	err = sandbox.Make(ctx, sandbox.MakeConfig{
		Dir:       target.SandboxRoot.Abs(),
		BinDir:    binDir,
		Bin:       bin,
		Files:     append(srcRec.Src(), restoreSrcRec.Src()...),
		LinkFiles: linkSrcRec.Src(),
		FilesTar:  append(srcRec.SrcTar(), restoreSrcRec.SrcTar()...),
	})
	if err != nil {
		return nil, err
	}

	traceSrcEnv := utils.TraceTiming("Building src env")

	env := make(map[string]string)
	env["TARGET"] = target.FQN
	env["PACKAGE"] = target.Package.FullName
	env["ROOT"] = target.WorkdirRoot.Abs()
	env["SANDBOX"] = target.SandboxRoot.Abs()
	if !(target.SrcEnv.All == targetspec.FileEnvIgnore && len(target.SrcEnv.Named) == 0) {
		for name, paths := range envSrcRec.Named() {
			fileEnv := target.SrcEnv.Get(srcRecNameToDepName[name])
			if fileEnv == targetspec.FileEnvIgnore {
				continue
			}

			spaths := make([]string, 0)
			for _, path := range paths {
				switch fileEnv {
				case targetspec.FileEnvAbs:
					spaths = append(spaths, target.SandboxRoot.Join(path).Abs())
				case targetspec.FileEnvRelRoot:
					spaths = append(spaths, path)
				case targetspec.FileEnvRelPkg:
					p := "/root"
					rel, err := filepath.Rel(filepath.Join(p, target.Package.FullName), filepath.Join(p, path))
					if err != nil {
						return nil, err
					}
					rel = strings.TrimPrefix(rel, p)
					rel = strings.TrimPrefix(rel, "/")
					spaths = append(spaths, rel)
				default:
					panic("unhandled src_env: " + fileEnv)
				}
			}

			k := "SRC_" + strings.ToUpper(name)
			if name == "" {
				k = "SRC"
			}
			env[normalizeEnv(k)] = strings.Join(spaths, " ")
		}
	}

	traceSrcEnv()

	if target.OutEnv != targetspec.FileEnvIgnore {
		out := target.Out.WithRoot(target.SandboxRoot.Abs()).Named()
		namedOut := map[string][]string{}
		for name, paths := range out {
			for _, path := range paths {
				if utils.IsGlob(path.RelRoot()) {
					// Skip glob
					continue
				}

				if filepath.Base(path.RelRoot()) == "." {
					// Skip dot folder
					continue
				}

				if target.Sandbox {
					// Create the output folder, as a convenience
					err := fs.CreateParentDir(path.Abs())
					if err != nil {
						return nil, err
					}
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
						return nil, err
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
	}
	for _, tool := range target.Tools.Hosts {
		k := "TOOL_" + strings.ToUpper(tool.Name)
		env[normalizeEnv(k)], err = tool.ResolvedPath()
		if err != nil {
			return nil, err
		}
	}

	for k, expr := range target.RuntimeEnv {
		val, err := exprs.Exec(expr.Value, e.queryFunctions(expr.Target))
		if err != nil {
			return nil, fmt.Errorf("runtime env `%v`: %w", expr, err)
		}

		env[normalizeEnv(strings.ToUpper(k))] = val
	}

	for k, v := range target.Env {
		env[k] = v
	}

	return &runPrepare{
		Env:    env,
		BinDir: binDir,
	}, nil
}

func (e *TargetRunEngine) Run(ctx context.Context, rr TargetRunRequest, iocfg sandbox.IOConfig) (rerr error) {
	target := rr.Target

	ctx, rspan := e.SpanRun(ctx, target)
	defer func() {
		rspan.EndError(rerr)
	}()

	err := e.LinkTarget(target, NewTargets(0))
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
	if target.Timeout > 0 {
		var cancel context.CancelFunc
		ctx, cancel = context.WithTimeout(ctx, target.Timeout)
		defer cancel()
	}

	if err := ctx.Err(); err != nil {
		return err
	}

	if target.Cache.Enabled && !rr.Shell && !rr.NoCache {
		if len(rr.Args) > 0 {
			return fmt.Errorf("args are not supported with cache")
		}

		start := time.Now()
		defer func() {
			log.Debugf("%v done in %v", target.FQN, time.Since(start))
		}()

		cached, err := e.WarmAllTargetCache(ctx, target)
		if err != nil {
			return err
		}

		if cached {
			return nil
		}
	}

	rp, err := e.runPrepare(ctx, target)
	if err != nil {
		return fmt.Errorf("prepare: %w", err)
	}

	env := rp.Env
	binDir := rp.BinDir

	dir := filepath.Join(target.WorkdirRoot.Abs(), target.Package.FullName)
	if target.RunInCwd {
		if target.Cache.Enabled {
			return fmt.Errorf("%v cannot run in cwd and cache", target.FQN)
		}

		dir = e.Cwd
	}

	if iocfg.Stdout == nil || iocfg.Stderr == nil {
		target.LogFile = e.sandboxRoot(target).Join("log.txt").Abs()

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

	e.Status(TargetStatus(target, "Running..."))

	if target.IsGroup() && !rr.Shell {
		// Ignore
	} else if target.IsTextFile() {
		to := target.Out.All()[0].WithRoot(target.SandboxRoot.Abs()).Abs()

		err := fs.CreateParentDir(to)
		if err != nil {
			return err
		}

		imode, err := strconv.ParseInt(target.Run[1], 8, 32)
		if err != nil {
			return err
		}
		mode := os.FileMode(imode)

		err = os.WriteFile(to, target.FileContent, os.ModePerm)
		if err != nil {
			return err
		}

		err = os.Chmod(to, mode)
		if err != nil {
			return err
		}
	} else {
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
		if target.IsTool() {
			log.Tracef("%v is tool, replacing run", target.FQN)
			run = []string{target.ToolTarget().AbsPath()}
		} else {
			for _, s := range target.Run {
				out, err := exprs.Exec(s, e.queryFunctions(target))
				if err != nil {
					return fmt.Errorf("run `%v`: %w", s, err)
				}

				run = append(run, out)
			}
		}

		if rr.Shell {
			shellCmds := executor.ShellPrint(run)

			fmt.Println("Shell mode enabled, exit the shell to terminate")
			if len(shellCmds) > 0 {
				fmt.Println("The run alias is available to execute the below commands:")
				fmt.Printf("%v\n", shellCmds)

				f, err := os.CreateTemp(e.tmpRoot(target), "")
				if err != nil {
					return err
				}
				defer os.Remove(f.Name())

				_, err = io.WriteString(f, fmt.Sprintf("function run {\n%v\n}\nfunction show {\ndeclare -f run\n}", shellCmds))
				if err != nil {
					return err
				}

				executor, err = sandbox.BashShellExecutor(f.Name())
				if err != nil {
					return err
				}

				err = f.Close()
				if err != nil {
					return err
				}
			}
		}

		_, hasPathInEnv := env["PATH"]
		sandbox.AddPathEnv(env, binDir, target.Sandbox && !hasPathInEnv)

		execArgs, err := executor.ExecArgs(sandbox.ExecutorContext{
			Args:    run,
			CmdArgs: rr.Args,
			Env:     env,
		})
		if err != nil {
			return err
		}

		log.Debugf("exec: %v", execArgs)

		err = sandbox.FilterLongEnv(env, execArgs)
		if rerr != nil {
			return err
		}

		cmd := sandbox.Exec(sandbox.ExecConfig{
			Context:  ctx,
			BinDir:   binDir,
			Dir:      dir,
			Env:      env,
			IOConfig: iocfg,
			ExecArgs: execArgs,
		})

		espan := e.SpanRunExec(ctx, target)
		err = cmd.Run()
		espan.EndError(err)
		if err != nil {
			if cerr := ctx.Err(); cerr != nil {
				err = fmt.Errorf("%w: %v", cerr, err)
			}

			err := fmt.Errorf("exec: %w", err)

			if target.LogFile != "" {
				return ErrorWithLogFile{
					LogFile: target.LogFile,
					Err:     err,
				}
			}

			return err
		}
	}

	if rr.Shell {
		return nil
	}

	outRoot := &target.WorkdirRoot
	if target.OutInSandbox {
		outRoot = &target.SandboxRoot
	}

	err = e.populateActualFiles(ctx, target, outRoot.Abs())
	if err != nil {
		return fmt.Errorf("pop: %w", err)
	}

	err = e.storeCache(ctx, target, outRoot.Abs())
	if err != nil {
		return fmt.Errorf("cache: store: %w", err)
	}

	if target.Cache.Enabled && !e.DisableNamedCacheWrite {
		for _, cache := range e.Config.Cache {
			cache := cache

			if !cache.Write {
				continue
			}

			if !target.Cache.NamedEnabled(cache.Name) {
				continue
			}

			e.Pool.Schedule(ctx, &worker.Job{
				Name: fmt.Sprintf("cache %v %v", target.FQN, cache.Name),
				Do: func(w *worker.Worker, ctx context.Context) error {
					w.Status(TargetStatus(target, fmt.Sprintf("Pushing to %v cache...", cache.Name)))

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
		e.Status(TargetStatus(target, "Clearing sandbox..."))
		err = deleteDir(target.SandboxRoot.Abs(), false)
		if err != nil {
			return err
		}
	}

	err = e.postRunOrWarm(ctx, target, target.OutWithSupport.Names())
	if err != nil {
		return err
	}

	return nil
}

func (e *TargetRunEngine) postRunOrWarm(ctx context.Context, target *Target, outputs []string) error {
	_, err := e.postRunOrWarmCached(ctx, target, outputs)
	return err
}

func (e *TargetRunEngine) postRunOrWarmCached(ctx context.Context, target *Target, outputs []string) (bool, error) {
	err := target.postRunWarmLock.Lock()
	if err != nil {
		return false, err
	}

	defer func() {
		err := target.postRunWarmLock.Unlock()
		if err != nil {
			log.Errorf("Failed to unlock %v", err)
		}
	}()

	doneMarker := e.cacheDir(target).Join(inputHashFile).Abs()
	if !fs.PathExists(doneMarker) {
		return false, err
	}

	outDir := e.cacheDir(target).Join("_output")
	outDirHashPath := e.cacheDir(target).Join("_output_hash").Abs()

	// sanity check
	for _, name := range outputs {
		p := e.targetOutputTarFile(target, name)
		if !fs.PathExists(p) {
			return false, fmt.Errorf("%v does not exist", p)
		}
	}
	outDirHash := "2|" + strings.Join(outputs, ",")

	shouldExpand := false
	if !fs.PathExists(outDir.Abs()) {
		shouldExpand = true
	} else {
		b, err := os.ReadFile(outDirHashPath)
		if err != nil && !errors.Is(err, fs2.ErrNotExist) {
			return false, err
		}

		if len(b) > 0 && strings.TrimSpace(string(b)) != outDirHash {
			shouldExpand = true
		}
	}

	if len(outputs) == 0 {
		shouldExpand = false
	}

	if shouldExpand {
		e.Status(TargetStatus(target, "Expanding cache..."))
		tmpOutDir := e.cacheDir(target).Join("_output_tmp").Abs()

		err := os.RemoveAll(tmpOutDir)
		if err != nil {
			return false, err
		}

		err = os.MkdirAll(tmpOutDir, os.ModePerm)
		if err != nil {
			return false, err
		}

		untarDedup := sets.NewStringSet(0)

		for _, name := range outputs {
			tarf := e.targetOutputTarFile(target, name)
			err := tar.UntarWith(ctx, tarf, tmpOutDir, tar.UntarOptions{
				List:  true,
				Dedup: untarDedup,
			})
			if err != nil {
				return false, err
			}
		}

		err = os.RemoveAll(outDir.Abs())
		if err != nil {
			return false, err
		}

		err = os.Rename(tmpOutDir, outDir.Abs())
		if err != nil {
			return false, err
		}

		err = os.WriteFile(outDirHashPath, []byte(outDirHash), os.ModePerm)
		if err != nil {
			return false, err
		}
	}

	target.OutExpansionRoot = &outDir

	if len(target.OutWithSupport.All()) > 0 {
		e.Status(TargetStatus(target, "Hydrating output..."))
	}

	err = e.populateActualFilesFromTar(target)
	if err != nil {
		return false, fmt.Errorf("poptar: %w", err)
	}

	err = e.codegenLink(target)
	if err != nil {
		return false, err
	}

	if target.Cache.Enabled && !e.Config.DisableGC {
		e.Status(TargetStatus(target, "GC..."))

		err = e.GCTargets([]*Target{target}, nil, false)
		if err != nil {
			log.Errorf("gc: %v", err)
		}
	}

	return true, nil
}

func (e *TargetRunEngine) codegenLink(target *Target) error {
	if target.Codegen == "" {
		return nil
	}

	e.Status(TargetStatus(target, "Linking output..."))

	for name, paths := range target.Out.Named() {
		switch target.Codegen {
		case targetspec.CodegenCopy:
			tarPath := e.targetOutputTarFile(target, name)
			err := tar.Untar(context.Background(), tarPath, e.Root.Abs(), false)
			if err != nil {
				return err
			}
		case targetspec.CodegenLink:
			for _, path := range paths {
				from := path.WithRoot(target.OutExpansionRoot.Abs()).Abs()
				to := path.WithRoot(e.Root.Abs()).Abs()

				info, err := os.Lstat(to)
				if err != nil && !errors.Is(err, os.ErrNotExist) {
					return err
				}
				exists := err == nil

				if exists {
					isLink := info.Mode().Type() == os.ModeSymlink

					if !isLink {
						log.Warnf("linking codegen: %v already exists", to)
						continue
					}

					err := os.Remove(to)
					if err != nil {
						return err
					}
				}

				err = fs.CreateParentDir(to)
				if err != nil {
					return err
				}

				err = os.Symlink(from, to)
				if err != nil {
					return err
				}
			}
		}
	}

	return nil
}
