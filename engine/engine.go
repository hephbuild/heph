package engine

import (
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	"github.com/c2fo/vfs/v6"
	vfsos "github.com/c2fo/vfs/v6/backend/os"
	log "github.com/sirupsen/logrus"
	"github.com/zeebo/xxh3"
	"hash"
	"heph/config"
	"heph/sandbox"
	"heph/utils"
	"heph/vfssimple"
	"heph/worker"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"syscall"
	"time"
)

type SourceFile struct {
	Path string
}

type SourceFiles []*SourceFile

func (sf SourceFiles) Find(p string) *SourceFile {
	for _, file := range sf {
		if file.Path == p {
			return file
		}
	}

	return nil
}

type Engine struct {
	Root       string
	HomeDir    string
	Config     Config
	LocalCache *vfsos.Location

	Context context.Context

	SourceFiles SourceFiles
	Packages    map[string]*Package

	Targets Targets
	Labels  []string

	dag *DAG
}

type Config struct {
	Profiles []string
	config.Config
	Cache []CacheConfig
}

type CacheConfig struct {
	config.Cache
	Name     string
	Read     bool
	Write    bool
	Location vfs.Location `yaml:"-"`
}

type RunStatus struct {
	Description string
}

func New(root string) *Engine {
	homeDir := filepath.Join(root, ".heph")

	log.Tracef("home dir %v", homeDir)

	err := os.MkdirAll(homeDir, os.ModePerm)
	if err != nil {
		log.Fatal(fmt.Errorf("create homedir %v: %w", homeDir, err))
	}

	cacheDir := filepath.Join(homeDir, "cache")

	loc, err := vfssimple.NewLocation("file://" + cacheDir + "/")
	if err != nil {
		log.Fatal(fmt.Errorf("cache location: %w", err))
	}

	return &Engine{
		Root:       root,
		HomeDir:    homeDir,
		Context:    context.Background(),
		LocalCache: loc.(*vfsos.Location),
	}
}

func (e *Engine) DAG() *DAG {
	return e.dag
}

func (e *Engine) hashFile(h hash.Hash, file PackagePath) {
	f, err := os.Open(file.Abs())
	if err != nil {
		panic(fmt.Errorf("hashFile: %w", err))
	}
	defer f.Close()

	_, err = io.Copy(h, f)
	if err != nil {
		panic(fmt.Errorf("hashFile: %w", err))
	}
}

func (e *Engine) hashInput(target *Target) string {
	start := time.Now()
	defer func() {
		log.Tracef("hashinput %v took %v", target.FQN, time.Since(start))
	}()

	h := xxh3.New()

	for _, dep := range target.Tools {
		dh := e.hashOutput(dep.Target)

		h.Write([]byte(dep.Name))
		h.Write([]byte(dh))
	}

	for _, dep := range target.HashDeps.Targets {
		dh := e.hashOutput(dep)

		h.Write([]byte(dh))
	}

	for _, dep := range target.HashDeps.Files {
		e.hashFile(h, dep)
	}

	for _, cmd := range target.Runnable.Cmds {
		h.Write([]byte(cmd))
	}

	for _, file := range target.CachedFiles {
		h.Write([]byte(file.RelRoot()))
	}

	envEntries := make([]string, 0)
	for k, v := range target.Env {
		envEntries = append(envEntries, k+v)
	}

	sort.Strings(envEntries)

	for _, e := range envEntries {
		h.Write([]byte(e))
	}

	hb := h.Sum128().Bytes()

	return hex.EncodeToString(hb[:])
}

func (e *Engine) hashOutput(target *Target) string {
	start := time.Now()
	defer func() {
		log.Tracef("hashoutput %v took %v", target.FQN, time.Since(start))
	}()

	h := xxh3.New()

	for _, file := range target.ActualFilesOut() {
		e.hashFile(h, file)
	}

	hb := h.Sum128().Bytes()

	return hex.EncodeToString(hb[:])
}

type TargetFailedError struct {
	Target *Target
	Err    error
}

func (t TargetFailedError) Error() string {
	return t.Err.Error()
}

func (e *Engine) ScheduleTarget(ctx context.Context, pool *worker.Pool, target *Target) error {
	ancestors, err := e.DAG().GetAncestors(target)
	if err != nil {
		return err
	}

	log.Tracef("Scheduling %v", target.FQN)

	pool.Schedule(&worker.Job{
		ID: target.FQN,
		Wait: func(ctx context.Context) {
			select {
			case <-ctx.Done():
			case <-ancestors.WaitAllRan():
			}
			return
		},
		Do: func(w *worker.Worker, ctx context.Context) error {
			w.Status(target.FQN)

			e := TargetRunEngine{
				Engine:  e,
				Pool:    pool,
				Worker:  w,
				Context: ctx,
			}

			err := e.Run(target, sandbox.IOConfig{})
			if err != nil {
				return TargetFailedError{
					Target: target,
					Err:    err,
				}
			}

			return nil
		},
	})

	return nil
}

func (e *Engine) ScheduleTargetDeps(ctx context.Context, pool *worker.Pool, target *Target) (Targets, error) {
	ancestors, err := e.DAG().GetAncestors(target)
	if err != nil {
		return nil, err
	}

	for _, target := range ancestors {
		err := e.ScheduleTarget(ctx, pool, target)
		if err != nil {
			return nil, err
		}
	}

	return ancestors, nil
}

func (e *Engine) populateActualFilesOut(target *Target) error {
	empty, err := utils.IsDirEmpty(target.OutRoot.Abs)
	if err != nil {
		return fmt.Errorf("collect output: isempty: %w", err)
	}

	if empty {
		target.actualFilesOut = make([]PackagePath, 0)
		return nil
	}

	outfiles := target.OutFilesInOutRoot()

	target.actualFilesOut = make([]PackagePath, 0)
	for _, file := range outfiles {
		f, err := os.Stat(file.Abs())
		if err != nil {
			return err
		}

		if f.IsDir() {
			err := filepath.WalkDir(file.Abs(), func(path string, info os.DirEntry, err error) error {
				if err != nil {
					return err
				}

				if info.IsDir() {
					return nil
				}

				rel, err := filepath.Rel(file.Abs(), path)
				if err != nil {
					return err
				}

				target.actualFilesOut = append(target.actualFilesOut, PackagePath{
					Package: file.Package,
					Path:    filepath.Join(file.Path, rel),
					Root:    file.Root,
				})

				return nil
			})
			if err != nil {
				return fmt.Errorf("collect output: %w", err)
			}
		} else {
			target.actualFilesOut = append(target.actualFilesOut, file)
		}
	}

	sort.SliceStable(target.actualFilesOut, func(i, j int) bool {
		return target.actualFilesOut[i].RelRoot() < target.actualFilesOut[j].RelRoot()
	})

	return nil
}

func (e *Engine) sandboxRoot(target *Target) string { // TODO: PackagePath
	return filepath.Join(e.HomeDir, "sandbox", target.Package.FullName, target.Name)
}

func (e *Engine) Clean(async bool) error {
	return deleteDir(e.HomeDir, async)
}

func (e *Engine) CleanTarget(target *Target, async bool) error {
	sandboxDir := e.sandboxRoot(target)
	err := deleteDir(sandboxDir, async)
	if err != nil {
		return err
	}

	cacheDir := e.cacheDir(target, "")
	err = deleteDir(cacheDir, async)
	if err != nil {
		return err
	}

	return nil
}

func (e *Engine) CleanTargetLock(target *Target) error {
	err := os.RemoveAll(target.runLock.Path())
	if err != nil {
		return err
	}

	err = os.RemoveAll(target.cacheLock.Path())
	if err != nil {
		return err
	}

	return nil
}

func deleteDir(dir string, async bool) error {
	rm, err := exec.LookPath("rm")
	if err != nil {
		return err
	}

	mv, err := exec.LookPath("mv")
	if err != nil {
		return err
	}

	if !utils.PathExists(dir) {
		return nil // not an error, just don't need to do anything.
	}

	log.Tracef("Deleting %v", dir)

	newDir := utils.RandPath("heph", "")

	out, err := exec.Command(mv, dir, newDir).CombinedOutput()
	if err != nil {
		return fmt.Errorf("mv: %v %v", err, string(out))
	}

	if async {
		// Note that we can't fork() directly and continue running Go code, but ForkExec() works okay.
		// Hence why we're using rm rather than fork() + os.RemoveAll.
		_, err = syscall.ForkExec(rm, []string{rm, "-rf", newDir}, nil)
		return err
	}

	out, err = exec.Command(rm, "-rf", newDir).CombinedOutput()
	if err != nil {
		log.Error("Failed to remove directory: %s", string(out))
	}
	return err
}

func (e *Engine) parseConfigs() error {
	log.Tracef("Profiles: %v", e.Config.Profiles)

	mainCfg, err := config.Parse(filepath.Join(e.Root, ".hephconfig"))
	if err != nil {
		return err
	}

	localCfg, err := config.Parse(filepath.Join(e.Root, ".hephconfig.local"))
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}

	cfg := mainCfg.Merge(localCfg)

	for _, profile := range e.Config.Profiles {
		profileCfg, err := config.Parse(filepath.Join(e.Root, ".hephconfig."+profile))
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}

		cfg = cfg.Merge(profileCfg)
	}

	// Finalize config

	cfg.BuildFiles.Ignore = append(cfg.BuildFiles.Ignore, ".heph")

	for k, cache := range cfg.Cache {
		if cache.Read == nil {
			v := true
			cache.Read = &v
		}
		if cache.Write == nil {
			v := false
			cache.Write = &v
		}
		cfg.Cache[k] = cache
	}

	e.Config.Config = cfg

	for name, cache := range cfg.Cache {
		loc, err := vfssimple.NewLocation(cache.URI)
		if err != nil {
			return fmt.Errorf("cache %v :%w", name, err)
		}

		e.Config.Cache = append(e.Config.Cache, CacheConfig{
			Name:     name,
			Cache:    cache,
			Read:     *cache.Read,
			Write:    *cache.Write,
			Location: loc,
		})
	}

	return nil
}

func (e *Engine) HasLabel(label string) bool {
	for _, l := range e.Labels {
		if l == label {
			return true
		}
	}

	return false
}

func (e *Engine) registerLabels(labels []string) {
	for _, label := range labels {
		if !e.HasLabel(label) {
			e.Labels = append(e.Labels, label)
		}
	}
}
