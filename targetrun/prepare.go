package targetrun

import (
	"context"
	"encoding/json"
	"fmt"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/exprs"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/hephprovider"
	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/lcache"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/observability"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/platform"
	"github.com/hephbuild/heph/sandbox"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/finalizers"
	"github.com/hephbuild/heph/utils/instance"
	"github.com/hephbuild/heph/utils/locks"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/utils/xmath"
	"github.com/hephbuild/heph/worker2"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"strings"
)

type Runner struct {
	Root              *hroot.State
	Observability     *observability.Observability
	Finalizers        *finalizers.Finalizers
	LocalCache        *lcache.LocalCacheState
	PlatformProviders []platform.PlatformProvider
	GetFlowID         func() string
	QueryFunctions    func(*graph.Target) map[string]exprs.Func
	Cwd               string
	Config            *config.Config
	Pool              *worker2.Engine
}

type Target struct {
	*graph.Target
	SandboxRoot     xfs.Path
	SandboxLock     locks.Locker
	SandboxTreeRoot xfs.Path
	WorkdirRoot     xfs.Path
	OutRoot         xfs.Path
	Env             map[string]string
	BinDir          string
	Executor        platform.Executor
}

func (e *Runner) sandboxRoot(specer specs.Specer) xfs.Path {
	target := specer.Spec()

	name := lcache.SanitizeTargetName(target.Name)

	folder := "__target_" + name
	if target.ConcurrentExecution {
		folder = "__target_tmp_" + instance.UID + "_" + name
	}

	p := e.Root.Home.Join("sandbox", target.Package.Path, folder)

	if target.ConcurrentExecution {
		e.Finalizers.RegisterRemove(p.Abs())
	}

	return p
}

func (e *Runner) tmpTargetRoot(target specs.Specer) xfs.Path {
	spec := target.Spec()
	return e.Root.Tmp.Join(spec.Package.Path, "__target_"+spec.Name)
}

func (e *Runner) chooseExecutor(labels map[string]string, options map[string]interface{}, providers []platform.PlatformProvider) (platform.Executor, error) {
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

	b, _ := json.Marshal(labels)

	return nil, fmt.Errorf("no platform available for %s", b)
}

func (e *Runner) createFile(target specs.Specer, name, path string, rec *SrcRecorder, fun func(writer io.Writer) error) (error, func()) {
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

func (e *Runner) toolAbsPath(tt graph.TargetTool) string {
	return tt.File.WithRoot(e.LocalCache.Metas.Find(tt.Target).OutExpansionRoot().Abs()).Abs()
}

func (e *Runner) runPrepare(ctx context.Context, target *graph.Target, rr Request) (_ *Target, rerr error) {
	ctx, span := e.Observability.SpanRunPrepare(ctx, target)
	defer span.EndError(rerr)

	status.Emit(ctx, tgt.TargetStatus(target, "Preparing..."))

	rtarget := &Target{
		Target:          target,
		SandboxRoot:     e.sandboxRoot(target),
		SandboxTreeRoot: e.SandboxTreeRoot(target),
	}
	rtarget.SandboxLock = locks.NewFlock(target.Addr+" (sandbox)", rtarget.SandboxRoot.Join("lock.lock").Abs())

	rtarget.WorkdirRoot = rtarget.SandboxTreeRoot
	if !rtarget.Sandbox {
		rtarget.WorkdirRoot = e.Root.Root
	}

	rtarget.OutRoot = rtarget.WorkdirRoot
	if rtarget.OutInSandbox {
		rtarget.OutRoot = rtarget.SandboxTreeRoot
	}

	err := rtarget.SandboxLock.Lock(ctx)
	if err != nil {
		return nil, err
	}

	defer func() {
		if rerr != nil {
			err := rtarget.SandboxLock.Unlock()
			if err != nil {
				log.Errorf("failed to unlock: %v", err)
			}
		}
	}()

	log.Debugf("Preparing %v: %v", target.Addr, rtarget.WorkdirRoot.RelRoot())

	// Sanity checks
	for _, tool := range target.Tools.Targets {
		if !e.LocalCache.Metas.Find(tool.Target).HasActualOutFiles() {
			return nil, fmt.Errorf("%v: %v did not run being being used as a tool", target.Addr, tool.Target.Addr)
		}
	}

	for _, dep := range target.Deps.All().Targets {
		if !e.LocalCache.Metas.Find(dep.Target).HasActualOutFiles() {
			return nil, fmt.Errorf("%v: %v did not run being being used as a dep", target.Addr, dep.Target.Addr)
		}
	}

	for _, dep := range target.RuntimeDeps.All().Targets {
		if !e.LocalCache.Metas.Find(dep.Target).HasActualOutFiles() {
			return nil, fmt.Errorf("%v: %v did not run being being used as a runtimedep", target.Addr, dep.Target.Addr)
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
				ctx, e.Root.Tmp.Join("__heph", utils.Version).Abs(),
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
	srcRec := &SrcRecorder{Forward: envSrcRec}
	// Records symlinks that should be created
	linkSrcRec := &SrcRecorder{}
	binDir := e.sandboxRoot(target).Join("_bin").Abs()

	status.Emit(ctx, tgt.TargetStatus(target, "Creating sandbox..."))

	restoreSrcRec := &SrcRecorder{}
	if target.RestoreCache.Enabled && !rr.NoCache {
		if e.LocalCache.LatestCacheDirExists(target) {
			done := log.TraceTiming("Restoring cache")

			arts := ads.Map(target.OutWithSupport.Names(), func(name string) artifacts.Artifact {
				return target.Artifacts.OutTar(name)
			})
			if a, ok := target.Artifacts.GetRestoreCache(); ok {
				arts = append(arts, a)
			}

			for _, art := range arts {
				p, stats, err := e.LocalCache.LatestUncompressedPathFromArtifact(ctx, target, art)
				if err != nil {
					log.Debug("%v: skipping restore cache: out %v: %v", target.Addr, art.Name(), err)
					// We do not want partial restore
					restoreSrcRec.Reset()
					break
				}
				restoreSrcRec.AddTar(p, stats.Size)
			}

			done()
		}
	}

	traceFilesList := log.TraceTiming("Building sandbox files list")

	allDeps := target.Deps
	if !target.RuntimeDeps.Empty() {
		allDeps = target.Deps.Copy()
		allDeps = allDeps.Merge(target.RuntimeDeps)
		allDeps.Sort()
	}

	srcRecNameToDepName := make(map[string]string)
	for name, deps := range allDeps.Named() {
		for _, dep := range deps.Targets {
			dept := e.LocalCache.Metas.Find(dep.Target)

			if len(dept.ActualOutFiles().All()) == 0 {
				continue
			}

			if dep.Mode == specs.DepModeLink {
				outDir := e.LocalCache.Metas.Find(dept).OutExpansionRoot().Abs()
				for _, file := range dept.ActualOutFiles().Name(dep.Output).WithRoot(outDir) {
					linkSrcRec.Add("", file.Abs(), file.RelRoot(), "")
				}
			} else {
				art, ok := dept.Artifacts.OutTar2(dep.Output)
				if !ok {
					return nil, fmt.Errorf("artifact %v|%v not found: %v", dept.Addr, dep.Output, dept.ActualOutFiles().Names())
				}
				p, stats, err := e.LocalCache.UncompressedPathFromArtifact(ctx, dept, art)
				if err != nil {
					return nil, err
				}
				srcRec.AddTar(p, stats.Size)
			}

			srcName := name
			if dep.Name != "" {
				if srcName != "" {
					srcName += "_"
				}
				srcName += dep.Name
			}

			for _, file := range dept.ActualOutFiles().Name(dep.Output) {
				srcRecNameToDepName[srcName] = name
				envSrcRec.Add(srcName, "", file.RelRoot(), dep.Full())
			}
		}

		for _, file := range deps.Files {
			if e.LocalCache.IsCodegenLink(file.Abs()) {
				log.Tracef("%v: is old codegen link, deleting...", file.Abs())
				_ = os.Remove(file.Abs())
				continue
			}

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

			for name, deps := range allDeps.Named() {
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

	makeCfg := sandbox.MakeConfig{
		Dir:       rtarget.SandboxTreeRoot.Abs(),
		BinDir:    binDir,
		Bin:       bin,
		Files:     append(srcRec.Src(), restoreSrcRec.Src()...),
		LinkFiles: linkSrcRec.Src(),
		FilesTar:  append(srcRec.SrcTar(), restoreSrcRec.SrcTar()...),
		ProgressFiles: func(percent float64) {
			status.EmitInteractive(ctx, tgt.TargetStatus(target, xmath.FormatPercent("Preparing sandbox: copying files [P]...", percent)))
		},
		ProgressTars: func(percent float64) {
			status.EmitInteractive(ctx, tgt.TargetStatus(target, xmath.FormatPercent("Preparing sandbox: copying deps [P]...", percent)))
		},
		ProgressLinks: func(percent float64) {
			status.EmitInteractive(ctx, tgt.TargetStatus(target, xmath.FormatPercent("Preparing sandbox: linking deps [P]...", percent)))
		},
	}

	err = sandbox.Make(ctx, makeCfg)
	if err != nil {
		return nil, err
	}

	status.Emit(ctx, tgt.TargetStatus(target, "Creating sandbox..."))

	// Check if modtime have changed
	_, err = e.LocalCache.VerifyHashInput(target)
	if err != nil {
		return nil, err
	}

	traceSrcEnv := log.TraceTiming("Building src env")

	env := make(map[string]string)
	env["TARGET"] = target.Addr
	env["PACKAGE"] = target.Package.Path
	env["ROOT"] = rtarget.WorkdirRoot.Abs()
	env["SANDBOX"] = rtarget.SandboxTreeRoot.Abs()
	if !target.Cache.Enabled {
		mode := rr.Mode
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

	if target.SrcEnv.Default != specs.FileEnvIgnore {
		for name, paths := range envSrcRec.Named() {
			if strings.HasPrefix(name, "_") {
				continue
			}

			fileEnv := target.SrcEnv.Get(srcRecNameToDepName[name])
			if fileEnv == specs.FileEnvIgnore {
				continue
			}

			spaths, err := ads.MapE(
				paths,
				func(path string) (string, error) {
					return envPathFactory(fileEnv, target.Package, xfs.NewRelPath(path).WithRoot(rtarget.SandboxTreeRoot.Abs()))
				},
			)
			if err != nil {
				return nil, err
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
		out := target.Out.WithRoot(rtarget.SandboxTreeRoot.Abs()).Named()
		namedOut := make(map[string][]string, len(out))
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

				pathv, err := envPathFactory(target.OutEnv, target.Package, path)
				if err != nil {
					return nil, err
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

	if target.RestoreCache.Env != specs.FileEnvIgnore {
		k := "RESTORE_CACHE"

		paths, err := ads.MapE(
			target.RestoreCachePaths.WithRoot(rtarget.SandboxTreeRoot.Abs()),
			func(path xfs.Path) (string, error) {
				return envPathFactory(target.RestoreCache.Env, target.Package, path)
			},
		)
		if err != nil {
			return nil, err
		}

		env[normalizeEnv(k)] = strings.Join(paths, " ")
	}

	for _, tool := range target.Tools.Targets {
		k := "TOOL_" + strings.ToUpper(tool.Name)
		if tool.Name == "" {
			k = "TOOL"
		}
		env[normalizeEnv(k)] = bin[tool.Name]
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
		val, err := exprs.Exec(expr.Value, e.QueryFunctions(expr.Target))
		if err != nil {
			return nil, fmt.Errorf("runtime env `%v`: %w", expr, err)
		}

		env[k] = val
	}

	for k, v := range target.Env {
		env[k] = v
	}

	rtarget.Env = env
	rtarget.BinDir = binDir
	rtarget.Executor = executor

	return rtarget, nil
}

func (e *Runner) SandboxTreeRoot(target specs.Specer) xfs.Path {
	return e.sandboxRoot(target).Join("_dir")
}

var envRegex = regexp.MustCompile(`[^A-Za-z0-9_]+`)

func normalizeEnv(k string) string {
	return envRegex.ReplaceAllString(k, "_")
}

func envPathFactory(envMode string, pkg *packages.Package, path xfs.Path) (string, error) {
	switch envMode {
	case specs.FileEnvAbs:
		return path.Abs(), nil
	case specs.FileEnvRelRoot:
		return path.RelRoot(), nil
	case specs.FileEnvRelPkg:
		fakeRoot := "/root"
		rel, err := filepath.Rel(filepath.Join(fakeRoot, pkg.Path), filepath.Join(fakeRoot, path.RelRoot()))
		if err != nil {
			return "", err
		}
		rel = strings.TrimPrefix(rel, fakeRoot)
		rel = strings.TrimPrefix(rel, "/")
		return rel, nil
	default:
		panic("unhandled env mode: " + envMode)
	}
}
