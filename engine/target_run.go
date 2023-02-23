package engine

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"heph/exprs"
	"heph/hephprovider"
	log "heph/hlog"
	"heph/platform"
	"heph/sandbox"
	"heph/targetspec"
	"heph/tgt"
	"heph/utils"
	"heph/utils/fs"
	"heph/utils/sets"
	"heph/utils/tar"
	"heph/worker"
	"io"
	fs2 "io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"time"
)

func NewTargetRunEngine(e *Engine, status func(s worker.Status)) TargetRunEngine {
	return TargetRunEngine{
		Engine: e,
		Status: status,
	}
}

type TargetRunEngine struct {
	*Engine
	Status func(s worker.Status)
}

func (e *Engine) tmpRoot(elem ...string) fs.Path {
	return e.HomeDir.Join("tmp").Join(elem...)
}

func (e *Engine) tmpTargetRoot(target *Target) fs.Path {
	return e.HomeDir.Join("tmp", target.Package.FullName, "__target_"+target.Name)
}

func (e *Engine) lockPath(target *Target, resource string) string {
	return e.tmpTargetRoot(target).Join(resource + ".lock").Abs()
}

var envRegex = regexp.MustCompile(`[^A-Za-z0-9_]+`)

func normalizeEnv(k string) string {
	return envRegex.ReplaceAllString(k, "_")
}

func (e *TargetRunEngine) createFile(target *Target, name, path string, rec *SrcRecorder, fun func(writer io.Writer) error) (error, func()) {
	tmppath := e.tmpTargetRoot(target).Join(name).Abs()

	err := fs.CreateParentDir(tmppath)
	if err != nil {
		return err, func() {}
	}

	f, err := os.Create(tmppath)
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
	Env      map[string]string
	BinDir   string
	Executor platform.Executor
}

func (e *TargetRunEngine) toolAbsPath(tt tgt.TargetTool) string {
	return tt.File.WithRoot(e.Targets.Find(tt.Target.FQN).OutExpansionRoot.Abs()).Abs()
}

func (e *TargetRunEngine) runPrepare(ctx context.Context, target *Target, mode string) (_ *runPrepare, rerr error) {
	log.Debugf("Preparing %v: %v", target.FQN, target.WorkdirRoot.RelRoot())

	span := e.SpanRunPrepare(ctx, target)
	defer func() {
		span.EndError(rerr)
	}()

	// Sanity checks
	for _, tool := range target.Tools.Targets {
		if e.Targets.Find(tool.Target.FQN).actualOutFiles == nil {
			panic(fmt.Sprintf("%v: %v did not run being being used as a tool", target.FQN, tool.Target.FQN))
		}
	}

	for _, dep := range target.Deps.All().Targets {
		if e.Targets.Find(dep.Target.FQN).actualOutFiles == nil {
			panic(fmt.Sprintf("%v: %v did not run being being used as a dep", target.FQN, dep.Target.FQN))
		}
	}

	plat := target.Platforms[0]
	executor, err := e.chooseExecutor(plat.Labels, plat.Options, e.PlatformProviders)
	if err != nil {
		return nil, err
	}

	bin := map[string]string{}
	for _, t := range target.Tools.Targets {
		bin[t.Name] = e.toolAbsPath(t)
	}

	var hephDistRoot string
	for _, t := range target.Tools.Hosts {
		var err error
		if t.BinName == "heph" {
			bin[t.Name], hephDistRoot, err = hephprovider.GetHephPath(
				e.HomeDir.Join("tmp", "__heph", utils.Version).Abs(),
				executor.Os(), executor.Arch(), utils.Version,
				true, /*!platform.HasHostFsAccess(executor)*/
			)
		} else {
			bin[t.Name], err = t.ResolvedPath()
		}

		if err != nil {
			return nil, err
		}
	}

	// Allow building from within a sandbox, especially useful for isolated e2e
	if utils.HephFromPath && utils.IsDevVersion() {
		if target.Tools.HasHeph() {
			var err error
			bin["go"], err = exec.LookPath("go")
			if err != nil {
				return nil, err
			}
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
		latestDir := e.cacheDirForHash(target, "latest")

		if fs.PathExists(latestDir.Abs()) {
			done := utils.TraceTiming("Restoring cache")

			for _, name := range target.OutWithSupport.Names() {
				p := latestDir.Join(target.artifacts.OutTar(name).Name()).Abs()
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
			deplength += len(e.Targets.Find(dep.Target.FQN).ActualOutFiles().Name(dep.Output))
		}
		deplength += len(deps.Files)

		length += deplength
	}

	traceFilesList := utils.TraceTiming("Building sandbox files list")

	srcRecNameToDepName := make(map[string]string, length)
	for name, deps := range target.Deps.Named() {
		for _, dep := range deps.Targets {
			dept := e.Targets.Find(dep.Target.FQN)

			if len(dept.ActualOutFiles().All()) == 0 {
				continue
			}

			if dep.Mode == targetspec.TargetSpecDepModeLink {
				for _, file := range dept.ActualOutFiles().Name(dep.Output) {
					linkSrcRec.Add("", file.Abs(), file.RelRoot(), "")
				}
			} else {
				tarFile := e.cacheDir(dept).Join(dept.artifacts.OutTar(dep.Output).Name()).Abs()
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
	if !target.Cache.Enabled {
		mode := mode
		if mode == "" {
			mode = "run"
		}
		env["HEPH_MODE"] = mode
	}
	if target.Tools.HasHeph() {
		// Forward heph variables inside the sandbox
		forward := []string{
			"HEPH_PROFILES",
			"HEPH_FROM_PATH",
			hephprovider.EnvSrcRoot,
			hephprovider.EnvDistRoot,
			hephprovider.EnvDistNoVersion,
		}
		for _, k := range forward {
			if value, ok := os.LookupEnv(k); ok {
				env[k] = value
			}
		}
	}
	if hephDistRoot != "" {
		env[hephprovider.EnvDistRoot] = hephDistRoot
	}

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
		env[normalizeEnv(k)] = e.toolAbsPath(tool)

		if tool.Target == nil {
			continue
		}
	}
	for _, tool := range target.Tools.Hosts {
		k := "TOOL_" + strings.ToUpper(tool.Name)
		env[normalizeEnv(k)] = bin[tool.Name]
	}

	for _, k := range target.RuntimePassEnv {
		v, ok := os.LookupEnv(k)
		if !ok {
			continue
		}

		env[k] = v
	}

	for k, expr := range target.RuntimeEnv {
		val, err := exprs.Exec(expr.Value, e.queryFunctions(e.Targets.Find(expr.Target.FQN)))
		if err != nil {
			return nil, fmt.Errorf("runtime env `%v`: %w", expr, err)
		}

		env[k] = val
	}

	for k, v := range target.Env {
		env[k] = v
	}

	return &runPrepare{
		Env:      env,
		BinDir:   binDir,
		Executor: executor,
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
	err = target.runLock.Lock(ctx)
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

		cached, err := e.pullOrGetCacheAndPost(ctx, target, target.OutWithSupport.Names(), true)
		if err != nil {
			return err
		}

		if cached {
			return nil
		}
	}

	rp, err := e.runPrepare(ctx, target, rr.Mode)
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

	e.Status(TargetStatus(target, "Running..."))

	logFilePath := ""

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
		var entrypoint platform.Entrypoint
		switch target.Entrypoint {
		case targetspec.EntrypointBash:
			entrypoint = platform.BashEntrypoint
		case targetspec.EntrypointSh:
			entrypoint = platform.ShEntrypoint
		case targetspec.EntrypointExec:
			entrypoint = platform.ExecEntrypoint
		default:
			panic("unhandled entrypoint: " + target.Entrypoint)
		}

		run := make([]string, 0)
		if target.IsTool() {
			log.Tracef("%v is tool, replacing run", target.FQN)
			run = append(target.Run[1:], e.toolAbsPath(target.ToolTarget()))
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
			if _, ok := env["TERM"]; !ok {
				env["TERM"] = os.Getenv("TERM")
			}
		}

		_, hasPathInEnv := env["PATH"]
		sandbox.AddPathEnv(env, binDir, target.Sandbox && !hasPathInEnv)

		var logFile *os.File
		if iocfg.Stdout == nil || iocfg.Stderr == nil {
			logFilePath = e.sandboxRoot(target).Join(target.artifacts.Log.Name()).Abs()

			logFile, err = os.Create(logFilePath)
			if err != nil {
				return err
			}

			if iocfg.Stdout == nil {
				iocfg.Stdout = logFile
			}

			if iocfg.Stderr == nil {
				iocfg.Stderr = logFile
			}
		}

		execCtx := ctx
		if target.Timeout > 0 {
			var cancel context.CancelFunc
			execCtx, cancel = context.WithTimeout(ctx, target.Timeout)
			defer cancel()
		}

		espan := e.SpanRunExec(ctx, target)
		err = platform.Exec(
			execCtx,
			rp.Executor,
			entrypoint,
			e.tmpTargetRoot(target).Abs(),
			platform.ExecOptions{
				WorkDir:  dir,
				BinDir:   binDir,
				HomeDir:  e.HomeDir.Abs(),
				Target:   target.TargetSpec,
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
		espan.EndError(err)
		if err != nil {
			if rr.Shell {
				log.Debugf("exec: %v", err)
				return nil
			}

			if cerr := ctx.Err(); cerr != nil {
				err = fmt.Errorf("%w: %v", cerr, err)
			}

			err := fmt.Errorf("exec: %w", err)

			if logFilePath != "" {
				return ErrorWithLogFile{
					LogFile: logFilePath,
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

	err = e.storeCache(ctx, target, outRoot.Abs(), logFilePath)
	if err != nil {
		return fmt.Errorf("cache: store: %w", err)
	}

	if !target.Cache.Enabled && !rr.PreserveCache {
		e.RegisterRemove(e.cacheDir(target).Abs())
	}

	if target.Cache.Enabled && !e.DisableNamedCacheWrite {
		orderedCaches, err := e.OrderedCaches(ctx)
		if err != nil {
			return err
		}

		for _, cache := range orderedCaches {
			if !cache.Write {
				continue
			}

			if !target.Cache.NamedEnabled(cache.Name) {
				continue
			}

			j := e.scheduleStoreExternalCache(ctx, target, cache)

			if poolDeps := ForegroundWaitGroup(ctx); poolDeps != nil {
				poolDeps.Add(j)
			}
		}
	}

	if !e.Config.KeepSandbox {
		e.Pool.Schedule(ctx, &worker.Job{
			Name: fmt.Sprintf("clear sandbox %v", target.FQN),
			Do: func(w *worker.Worker, ctx context.Context) error {
				w.Status(TargetStatus(target, "Clearing sandbox..."))
				err = deleteDir(target.SandboxRoot.Abs(), false)
				if err != nil {
					return fmt.Errorf("clear sandbox: %w", err)
				}

				return nil
			},
		})
	}

	err = e.postRunOrWarm(ctx, target, target.OutWithSupport.Names(), true)
	if err != nil {
		return err
	}

	return nil
}

func (e *TargetRunEngine) chooseExecutor(labels map[string]string, options map[string]interface{}, providers []PlatformProvider) (platform.Executor, error) {
	for _, p := range providers {
		executor, err := p.NewExecutor(labels, options)
		if err != nil {
			log.Errorf("%v: %v", p.Name, err)
			return nil, err
		}

		if executor == nil {
			continue
		}

		return executor, nil
	}

	return nil, fmt.Errorf("no platform available for %v", labels)
}

func (e *TargetRunEngine) postRunOrWarm(ctx context.Context, target *Target, outputs []string, runGc bool) error {
	err := target.postRunWarmLock.Lock(ctx)
	if err != nil {
		return err
	}

	defer func() {
		err := target.postRunWarmLock.Unlock()
		if err != nil {
			log.Errorf("Failed to unlock %v", err)
		}
	}()

	cacheDir := e.cacheDir(target)

	doneMarker := cacheDir.Join(target.artifacts.InputHash.Name()).Abs()
	if !fs.PathExists(doneMarker) {
		return err
	}

	outDir := e.cacheDir(target).Join("_output")
	outDirHashPath := e.cacheDir(target).Join("_output_hash").Abs()

	// sanity check
	for _, name := range outputs {
		p := cacheDir.Join(target.artifacts.OutTar(name).Name()).Abs()
		if !fs.PathExists(p) {
			panic(fmt.Errorf("%v does not exist", p))
		}
	}
	outDirHash := "2|" + strings.Join(outputs, ",")

	shouldExpand := false
	if !fs.PathExists(outDir.Abs()) {
		shouldExpand = true
	} else {
		b, err := os.ReadFile(outDirHashPath)
		if err != nil && !errors.Is(err, fs2.ErrNotExist) {
			return err
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
			return err
		}

		err = os.MkdirAll(tmpOutDir, os.ModePerm)
		if err != nil {
			return err
		}

		untarDedup := sets.NewStringSet(0)

		for _, name := range outputs {
			tarf := cacheDir.Join(target.artifacts.OutTar(name).Name()).Abs()
			err := tar.UntarWith(ctx, tarf, tmpOutDir, tar.UntarOptions{
				List:  true,
				Dedup: untarDedup,
			})
			if err != nil {
				return err
			}
		}

		err = os.RemoveAll(outDir.Abs())
		if err != nil {
			return err
		}

		err = os.Rename(tmpOutDir, outDir.Abs())
		if err != nil {
			return err
		}

		err = os.WriteFile(outDirHashPath, []byte(outDirHash), os.ModePerm)
		if err != nil {
			return err
		}
	}

	target.OutExpansionRoot = &outDir

	if len(target.OutWithSupport.All()) > 0 {
		e.Status(TargetStatus(target, "Hydrating output..."))
	}

	err = e.populateActualFilesFromTar(target)
	if err != nil {
		return fmt.Errorf("poptar: %w", err)
	}

	err = e.codegenLink(ctx, target)
	if err != nil {
		return err
	}

	err = e.linkLatestCache(target, cacheDir.Abs())
	if err != nil {
		return nil
	}

	if runGc {
		err = e.gc(ctx, target)
		if err != nil {
			return err
		}
	}

	return nil
}

func (e *TargetRunEngine) gc(ctx context.Context, target *Target) error {
	if target.Cache.Enabled && !e.Config.DisableGC {
		e.Status(TargetStatus(target, "GC..."))

		err := e.GCTargets([]*Target{target}, nil, false)
		if err != nil {
			log.Errorf("gc: %v", err)
		}
	}

	return nil
}

func (e *TargetRunEngine) codegenLink(ctx context.Context, target *Target) error {
	if target.Codegen == "" {
		return nil
	}

	e.Status(TargetStatus(target, "Linking output..."))

	for name, paths := range target.Out.Named() {
		if err := ctx.Err(); err != nil {
			return err
		}

		switch target.Codegen {
		case targetspec.CodegenCopy:
			tarf := e.cacheDir(target).Join(target.artifacts.OutTar(name).Name()).Abs()
			err := tar.Untar(ctx, tarf, e.Root.Abs(), false)
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
