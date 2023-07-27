package engine

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	ptylib "github.com/creack/pty"
	"github.com/hephbuild/heph/exprs"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/hephprovider"
	"github.com/hephbuild/heph/lcache"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/platform"
	"github.com/hephbuild/heph/sandbox"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/tar"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/worker"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"time"
)

func (e *Engine) tmpRoot(elem ...string) xfs.Path {
	return e.Root.Home.Join("tmp").Join(elem...)
}

func (e *Engine) tmpTargetRoot(target specs.Specer) xfs.Path {
	spec := target.Spec()
	return e.Root.Home.Join("tmp", spec.Package.Path, "__target_"+spec.Name)
}

func (e *Engine) lockPath(target specs.Specer, resource string) string {
	return e.tmpTargetRoot(target).Join(resource + ".lock").Abs()
}

var envRegex = regexp.MustCompile(`[^A-Za-z0-9_]+`)

func normalizeEnv(k string) string {
	return envRegex.ReplaceAllString(k, "_")
}

func (e *Engine) createFile(target *Target, name, path string, rec *SrcRecorder, fun func(writer io.Writer) error) (error, func()) {
	tmppath := e.tmpTargetRoot(target).Join(name).Abs()

	err := xfs.CreateParentDir(tmppath)
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

func (e *Engine) toolAbsPath(tt graph.TargetTool) string {
	return tt.File.WithRoot(e.Targets.Find(tt.Target).OutExpansionRoot.Abs()).Abs()
}

func (e *Engine) runPrepare(ctx context.Context, target *Target, mode string) (_ *runPrepare, rerr error) {
	log.Debugf("Preparing %v: %v", target.FQN, target.WorkdirRoot.RelRoot())

	ctx, span := e.Observability.SpanRunPrepare(ctx, target.Target)
	defer span.EndError(rerr)

	// Sanity checks
	for _, tool := range target.Tools.Targets {
		if e.Targets.Find(tool.Target).actualOutFiles == nil {
			panic(fmt.Sprintf("%v: %v did not run being being used as a tool", target.FQN, tool.Target.FQN))
		}
	}

	for _, dep := range target.Deps.All().Targets {
		if e.Targets.Find(dep.Target).actualOutFiles == nil {
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
				e.tmpRoot("__heph", utils.Version).Abs(),
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

	log.Tracef("Bin %#v", bin)

	// Records all src as files (even tar) to be used for creating SRC vars later
	envSrcRec := &SrcRecorder{}
	// Records src that should be copied, tar & files
	srcRec := &SrcRecorder{Parent: envSrcRec}
	// Records symlinks that should be created
	linkSrcRec := &SrcRecorder{}
	sandboxRoot := e.sandboxRoot(target)
	binDir := sandboxRoot.Join("_bin").Abs()

	status.Emit(ctx, tgt.TargetStatus(target, "Creating sandbox..."))

	restoreSrcRec := &SrcRecorder{}
	if target.RestoreCache {
		latestDir := e.LocalCache.LatestCacheDir(target)

		if xfs.PathExists(latestDir.Abs()) {
			done := log.TraceTiming("Restoring cache")

			for _, name := range target.OutWithSupport.Names() {
				art := target.Artifacts.OutTar(name)
				p, err := lcache.UncompressedPathFromArtifact(ctx, target, art, latestDir.Abs())
				if err != nil {
					log.Errorf("restore cache: out %v|%v: %v", target.FQN, art.Name(), err)
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
			deplength += len(e.Targets.Find(dep.Target).ActualOutFiles().Name(dep.Output))
		}
		deplength += len(deps.Files)

		length += deplength
	}

	traceFilesList := log.TraceTiming("Building sandbox files list")

	srcRecNameToDepName := make(map[string]string, length)
	for name, deps := range target.Deps.Named() {
		for _, dep := range deps.Targets {
			dept := e.Targets.Find(dep.Target)

			if len(dept.ActualOutFiles().All()) == 0 {
				continue
			}

			if dep.Mode == specs.DepModeLink {
				for _, file := range dept.ActualOutFiles().Name(dep.Output) {
					linkSrcRec.Add("", file.Abs(), file.RelRoot(), "")
				}
			} else {
				art := dept.Artifacts.OutTar(dep.Output)
				p, err := e.LocalCache.UncompressedPathFromArtifact(ctx, dept, art)
				if err != nil {
					return nil, err
				}
				srcRec.AddTar(p)
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

	if target.GenDepsMeta {
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
	}

	// Save files modtime
	e.LocalCache.ResetCacheHashInput(target)
	_, err = e.LocalCache.HashInput(target)
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

	// Check if modtime have changed
	_, err = e.LocalCache.HashInputSafely(target)
	if err != nil {
		return nil, err
	}

	traceSrcEnv := log.TraceTiming("Building src env")

	env := make(map[string]string)
	env["TARGET"] = target.FQN
	env["PACKAGE"] = target.Package.Path
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
			"HEPH_CLOUD_TOKEN",
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

	if e.GetFlowID != nil {
		env["HEPH_FLOW_ID"] = e.GetFlowID()
	}
	if hephDistRoot != "" {
		env[hephprovider.EnvDistRoot] = hephDistRoot
	}

	if !(target.SrcEnv.Default == specs.FileEnvIgnore && len(target.SrcEnv.Named) == 0) {
		for name, paths := range envSrcRec.Named() {
			if strings.HasPrefix(name, "_") {
				continue
			}

			fileEnv := target.SrcEnv.Get(srcRecNameToDepName[name])
			if fileEnv == specs.FileEnvIgnore {
				continue
			}

			spaths := make([]string, 0, len(paths))
			for _, path := range paths {
				switch fileEnv {
				case specs.FileEnvAbs:
					spaths = append(spaths, target.SandboxRoot.Join(path).Abs())
				case specs.FileEnvRelRoot:
					spaths = append(spaths, path)
				case specs.FileEnvRelPkg:
					p := "/root"
					rel, err := filepath.Rel(filepath.Join(p, target.Package.Path), filepath.Join(p, path))
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

	if target.OutEnv != specs.FileEnvIgnore {
		out := target.Out.WithRoot(target.SandboxRoot.Abs()).Named()
		namedOut := map[string][]string{}
		for name, paths := range out {
			namedOut[name] = ads.GrowExtra(namedOut[name], len(paths))

			for _, path := range paths {
				if xfs.IsGlob(path.RelRoot()) {
					// Skip glob
					continue
				}

				if filepath.Base(path.RelRoot()) == "." {
					// Skip dot folder
					continue
				}

				if target.Sandbox {
					// Create the output folder, as a convenience
					err := xfs.CreateParentDir(path.Abs())
					if err != nil {
						return nil, err
					}
				}

				var pathv string
				switch target.OutEnv {
				case specs.FileEnvAbs:
					pathv = path.Abs()
				case specs.FileEnvRelRoot:
					pathv = path.RelRoot()
				case specs.FileEnvRelPkg:
					fakeRoot := "/root"
					rel, err := filepath.Rel(filepath.Join(fakeRoot, target.Package.Path), filepath.Join(fakeRoot, path.RelRoot()))
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
		val, err := exprs.Exec(expr.Value, e.queryFunctions(e.Targets.Find(expr.Target)))
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

func (e *Engine) WriteableCaches(ctx context.Context, target *Target) ([]graph.CacheConfig, error) {
	if !target.Cache.Enabled || e.DisableNamedCacheWrite {
		return nil, nil
	}

	wcs := make([]graph.CacheConfig, 0)
	for _, cache := range e.Config.Caches {
		if !cache.Write {
			continue
		}

		if !target.Cache.NamedEnabled(cache.Name) {
			continue
		}

		wcs = append(wcs, cache)
	}

	if len(wcs) == 0 {
		return nil, nil
	}

	orderedCaches, err := e.OrderedCaches(ctx)
	if err != nil {
		return nil, err
	}

	// Reset and re-add in order
	wcs = nil

	for _, cache := range orderedCaches {
		if !cache.Write {
			continue
		}

		if !target.Cache.NamedEnabled(cache.Name) {
			continue
		}

		wcs = append(wcs, cache)
	}

	return wcs, nil
}

func (e *Engine) Run(ctx context.Context, rr TargetRunRequest, iocfg sandbox.IOConfig) (rerr error) {
	target := e.Targets.Find(rr.Target)

	ctx, rspan := e.Observability.SpanRun(ctx, target.Target)
	defer rspan.EndError(rerr)

	err := e.Graph.LinkTarget(target.Target, nil)
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

		cached, err := e.pullOrGetCacheAndPost(ctx, target, target.OutWithSupport.Names(), true, false)
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

	dir := filepath.Join(target.WorkdirRoot.Abs(), target.Package.Path)
	if target.RunInCwd {
		if target.Cache.Enabled {
			return fmt.Errorf("%v cannot run in cwd and cache", target.FQN)
		}

		dir = e.Cwd
	}

	status.Emit(ctx, tgt.TargetStatus(target, "Running..."))

	var logFilePath string
	if target.IsGroup() && !rr.Shell {
		// Ignore
	} else if target.IsTextFile() {
		to := target.Out.All()[0].WithRoot(target.SandboxRoot.Abs()).Abs()

		err := xfs.CreateParentDir(to)
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
		case specs.EntrypointBash:
			entrypoint = platform.BashEntrypoint
		case specs.EntrypointSh:
			entrypoint = platform.ShEntrypoint
		case specs.EntrypointExec:
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

		execCtx, execSpan := e.Observability.SpanRunExec(execCtx, target.Target)

		obw := e.Observability.LogsWriter(execCtx)

		var logFile *os.File
		if !rr.Shell {
			logFilePath = e.sandboxRoot(target).Join("log.txt").Abs()

			logFile, err = os.Create(logFilePath)
			if err != nil {
				return err
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
					return err
				}
				defer clean()

				iocfg.Stdout = pty
			} else {
				iocfg.Stdout = outw
			}

			if !rr.NoPTY && isWriterTerminal(iocfg.Stderr) {
				pty, err, clean := createPty(errw, szch)
				if err != nil {
					return err
				}
				defer clean()

				iocfg.Stderr = pty
			} else {
				iocfg.Stderr = errw
			}
		}

		err = platform.Exec(
			execCtx,
			rp.Executor,
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
				return nil
			}

			if cerr := ctx.Err(); cerr != nil {
				if !errors.Is(cerr, err) {
					err = fmt.Errorf("%w: %v", cerr, err)
				}
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

	writeableCaches, err := e.WriteableCaches(ctx, target)
	if err != nil {
		return fmt.Errorf("wcs: %w", err)
	}

	allArtifacts := e.orderedArtifactProducers(target, outRoot.Abs(), logFilePath)

	err = e.LocalCache.StoreCache(ctx, target, allArtifacts, len(writeableCaches) > 0)
	if err != nil {
		return fmt.Errorf("cache: store: %w", err)
	}

	if !target.Cache.Enabled && !rr.PreserveCache {
		e.LocalCache.RegisterRemove(target)
	}

	if len(writeableCaches) > 0 {
		for _, cache := range writeableCaches {
			j := e.scheduleStoreExternalCache(ctx, target, cache)

			if poolDeps := ForegroundWaitGroup(ctx); poolDeps != nil {
				poolDeps.Add(j)
			}
		}
	}

	if !e.Config.Engine.KeepSandbox {
		e.Pool.Schedule(ctx, &worker.Job{
			Name: fmt.Sprintf("clear sandbox %v", target.FQN),
			Do: func(w *worker.Worker, ctx context.Context) error {
				status.Emit(ctx, tgt.TargetStatus(target, "Clearing sandbox..."))
				err = xfs.DeleteDir(target.SandboxRoot.Abs(), false)
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

func (e *Engine) chooseExecutor(labels map[string]string, options map[string]interface{}, providers []platform.PlatformProvider) (platform.Executor, error) {
	for _, p := range providers {
		executor, err := p.NewExecutor(labels, options)
		if err != nil {
			log.Errorf("%v: %v", p.Name, err)
			continue
		}

		if executor == nil {
			continue
		}

		return executor, nil
	}

	return nil, fmt.Errorf("no platform available for %v", labels)
}

func (e *Engine) postRunOrWarm(ctx context.Context, target *Target, outputs []string, runGc bool) error {
	err := target.postRunWarmLock.Lock(ctx)
	if err != nil {
		return fmt.Errorf("lock postrunwarm: %w", err)
	}

	defer func() {
		err := target.postRunWarmLock.Unlock()
		if err != nil {
			log.Errorf("Failed to unlock %v", err)
		}
	}()

	hash, err := e.LocalCache.HashInput(target)
	if err != nil {
		return err
	}

	outDir, err := e.LocalCache.Expand(ctx, target, outputs)
	if err != nil {
		return fmt.Errorf("expand: %w", err)
	}

	target.OutExpansionRoot = &outDir

	if len(target.OutWithSupport.All()) > 0 {
		status.Emit(ctx, tgt.TargetStatus(target, "Hydrating output..."))
	}

	err = e.populateActualFilesFromTar(ctx, target, outputs)
	if err != nil {
		return fmt.Errorf("poptar: %w", err)
	}

	err = e.codegenLink(ctx, target)
	if err != nil {
		return fmt.Errorf("codegenlink: %w", err)
	}

	err = e.LocalCache.LinkLatestCache(target, hash)
	if err != nil {
		return fmt.Errorf("linklatest: %w", err)
	}

	if runGc {
		err = e.gc(ctx, target.Target)
		if err != nil {
			log.Errorf("gc %v: %v", target.FQN, err)
		}
	}

	return nil
}

func (e *Engine) gc(ctx context.Context, target *graph.Target) error {
	if target.Cache.Enabled && e.Config.Engine.GC {
		status.Emit(ctx, tgt.TargetStatus(target, "GC..."))

		err := e.LocalCache.GCTargets([]*graph.Target{target}, nil, false)
		if err != nil {
			return err
		}
	}

	return nil
}

func (e *Engine) codegenLink(ctx context.Context, target *Target) error {
	if target.Codegen == "" {
		return nil
	}

	status.Emit(ctx, tgt.TargetStatus(target, "Linking output..."))

	for name, paths := range target.Out.Named() {
		if err := ctx.Err(); err != nil {
			return err
		}

		switch target.Codegen {
		case specs.CodegenCopy, specs.CodegenCopyNoExclude:
			tarf, err := e.LocalCache.UncompressedReaderFromArtifact(target.Artifacts.OutTar(name), target)
			if err != nil {
				return err
			}

			err = tar.UntarContext(ctx, tarf, e.Root.Root.Abs(), tar.UntarOptions{})
			_ = tarf.Close()
			if err != nil {
				return err
			}
		case specs.CodegenLink:
			for _, path := range paths {
				from := path.WithRoot(target.OutExpansionRoot.Abs()).Abs()
				to := path.WithRoot(e.Root.Root.Abs()).Abs()

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

				err = xfs.CreateParentDir(to)
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
