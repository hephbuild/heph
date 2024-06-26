package lcache

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/c2fo/vfs/v6"
	vfsos "github.com/c2fo/vfs/v6/backend/os"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/observability"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/finalizers"
	"github.com/hephbuild/heph/utils/locks"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/tar"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/utils/xmath"
	"github.com/hephbuild/heph/vfssimple"
	"github.com/hephbuild/heph/worker2"
	"io"
	"io/fs"
	"math"
	"os"
	"path/filepath"
	"sync"
)

type LocalCacheState struct {
	Location        *vfsos.Location
	Path            xfs.Path
	Targets         *graph.Targets
	Metas           *TargetMetas[*Target]
	Root            *hroot.State
	Observability   *observability.Observability
	Finalizers      *finalizers.Finalizers
	EnableGC        bool
	ParallelCaching bool
	Pool            *worker2.Engine
	CacheRW         bool
}

const LatestDir = "latest"

func NewState(root *hroot.State, pool *worker2.Engine, targets *graph.Targets, obs *observability.Observability, finalizers *finalizers.Finalizers, gc, parallelCaching, cacheRw bool) (*LocalCacheState, error) {
	cachePath := root.Home.Join("cache")
	loc, err := vfssimple.NewLocation("file://" + cachePath.Abs() + "/")
	if err != nil {
		return nil, fmt.Errorf("lcache location: %w", err)
	}

	s := &LocalCacheState{
		Location:        loc.(*vfsos.Location),
		Path:            cachePath,
		Targets:         targets,
		Root:            root,
		Observability:   obs,
		Finalizers:      finalizers,
		EnableGC:        gc,
		ParallelCaching: parallelCaching,
		CacheRW:         cacheRw,
		Pool:            pool,
		Metas: NewTargetMetas(func(k targetMetaKey) *Target {
			gtarget := targets.Find(k.addr)

			t := &Target{
				Target:                     gtarget,
				depsHash:                   k.depshash,
				inputHash:                  "",
				actualOutFiles:             nil,
				allArtifactsLock:           locks.NewFlock(gtarget.Addr+" (artifacts)", lockPath(root, gtarget, "all_artifacts")),
				artifactLocks:              nil, // Set after
				cacheHashInputTargetMutex:  sync.Mutex{},
				cacheHashOutputTargetMutex: maps.KMutex{},
				cacheHashOutput:            &maps.Map[string, string]{},
				cacheHashInputPathsModtime: nil,
				expandLock:                 locks.NewFlock(gtarget.Addr+" (expand)", lockPath(root, gtarget, "expand")),
				sharedStageRoot:            root.Home.Join("shared_stage", gtarget.Package.Path, gtarget.Name),
			}

			ts := t.Spec()

			t.artifactLocks = make(map[string]locks.Locker, len(t.Artifacts.All()))
			for _, artifact := range t.Artifacts.All() {
				resource := artifact.Name()

				p := lockPath(root, t, "cache_"+resource)

				t.artifactLocks[artifact.Name()] = locks.NewFlock(ts.Addr+" ("+resource+")", p)
			}

			return t
		}),
	}

	return s, nil
}

func (e *LocalCacheState) LockPath(target graph.Targeter, resource string) string {
	return lockPath(e.Root, target, resource)
}

func (e *LocalCacheState) StoreCache(ctx context.Context, ttarget graph.Targeter, arts []ArtifactWithProducer, compress bool) (rerr error) {
	target := ttarget.GraphTarget()

	if target.ConcurrentExecution {
		log.Debugf("%v concurrent execution, skipping storeCache", target.Addr)
		return nil
	}

	if target.Cache.Enabled {
		status.Emit(ctx, tgt.TargetStatus(target, "Caching..."))
	} else if len(target.Artifacts.Out) > 0 {
		status.Emit(ctx, tgt.TargetStatus(target, "Storing..."))
	}

	ctx, span := e.Observability.SpanLocalCacheStore(ctx, target)
	defer span.EndError(rerr)

	unlock, err := e.LockAllArtifacts(ctx, target)
	if err != nil {
		return err
	}
	defer unlock()

	if e.ParallelCaching {
		genDeps, err := e.ScheduleGenArtifacts(ctx, target, arts, compress)
		if err != nil {
			return err
		}

		err = worker2.WaitDep(ctx, genDeps)
		if err != nil {
			return err
		}
	} else {
		err := e.GenArtifacts(ctx, target, arts, compress)
		if err != nil {
			return err
		}
	}

	return nil
}

func (e *LocalCacheState) LinkLatestCache(target specs.Specer, hash string) error {
	latestDir := e.cacheDirForHash(target, LatestDir).Abs()
	fromDir := e.cacheDirForHash(target, hash).Abs()

	err := os.RemoveAll(latestDir)
	if err != nil {
		return err
	}

	err = xfs.CreateParentDir(latestDir)
	if err != nil {
		return err
	}

	err = os.Symlink(fromDir, latestDir)
	if err != nil && !errors.Is(err, os.ErrExist) {
		return err
	}

	return nil
}

func (e *LocalCacheState) ResetCacheHashInput(spec specs.Specer) {
	e.Metas.Delete(spec)
}

func (e *LocalCacheState) HasArtifact(ctx context.Context, target graph.Targeter, artifact artifacts.Artifact, skipSpan bool) (bool, error) {
	setCacheHit := func(bool) {}
	if !skipSpan {
		var span *observability.TargetArtifactCacheSpan
		ctx, span = e.Observability.SpanLocalCacheCheck(ctx, target.GraphTarget(), artifact)
		defer span.End()
		setCacheHit = func(v bool) {
			span.SetCacheHit(v)
		}
	}

	cacheDir, err := e.cacheDir(target)
	if err != nil {
		return false, err
	}

	for _, name := range []string{artifact.FileName(), artifact.GzFileName()} {
		p := cacheDir.Join(name).Abs()
		if xfs.PathExists(p) {
			setCacheHit(true)
			return true, nil
		}
	}

	setCacheHit(false)
	return false, nil
}

func (e *LocalCacheState) LatestArtifactManifest(ctx context.Context, target graph.Targeter, artifact artifacts.Artifact) (ArtifactManifest, bool) {
	return e.artifactManifestWithFallback(ctx, e.cacheDirForHash(target, LatestDir), target, artifact)
}

func (e *LocalCacheState) ArtifactManifest(ctx context.Context, target graph.Targeter, artifact artifacts.Artifact) (ArtifactManifest, bool, error) {
	dir, err := e.cacheDir(target)
	if err != nil {
		return ArtifactManifest{}, false, err
	}

	m, ok := e.artifactManifestWithFallback(ctx, dir, target, artifact)
	return m, ok, nil
}

func (e *LocalCacheState) artifactManifestWithFallback(ctx context.Context, dir xfs.Path, target graph.Targeter, artifact artifacts.Artifact) (ArtifactManifest, bool) {
	stats, ok := e.artifactManifest(ctx, dir, target, artifact)

	if !ok {
		// Try to get the size from local state

		p := dir.Join(artifact.FileName()).Abs()

		info, _ := os.Lstat(p)
		if info != nil {
			stats.Size = info.Size()
		}
	}

	return stats, ok
}

func (e *LocalCacheState) artifactManifest(ctx context.Context, dir xfs.Path, target graph.Targeter, artifact artifacts.Artifact) (ArtifactManifest, bool) {
	p := dir.Join(artifact.ManifestFileName()).Abs()

	b, err := os.ReadFile(p)
	if err != nil {
		if !errors.Is(err, os.ErrNotExist) {
			log.Warnf("%v: %v: manifest: %v", target.Spec().Addr, artifact.Name(), err)
		}
		return ArtifactManifest{}, false
	}

	var m ArtifactManifest
	err = json.Unmarshal(b, &m)
	if err != nil {
		log.Warnf("%v: %v: manifest: %v (%s)", target.Spec().Addr, artifact.Name(), err, b)
		return ArtifactManifest{}, false
	}

	return m, true
}

func (e *LocalCacheState) GetLocalCache(ctx context.Context, target *graph.Target, outputs []string, withRestoreCache, onlyMeta, skipSpan, uncompress bool) (bool, error) {
	ok, err := e.HasArtifact(ctx, target, target.Artifacts.InputHash, skipSpan)
	if !ok || err != nil {
		return false, nil
	}

	for _, output := range outputs {
		ok, err := e.HasArtifact(ctx, target, target.Artifacts.OutHash(output), skipSpan)
		if !ok || err != nil {
			return false, err
		}

		if !onlyMeta {
			art := target.Artifacts.OutTar(output)

			ok, err := e.HasArtifact(ctx, target, art, skipSpan)
			if !ok || err != nil {
				return false, err
			}

			if uncompress {
				_, _, err := e.UncompressedPathFromArtifact(ctx, target, art)
				if err != nil {
					return false, err
				}
			}
		}
	}

	if art, ok := target.Artifacts.GetRestoreCache(); !onlyMeta && withRestoreCache && ok {
		ok, err := e.HasArtifact(ctx, target, art, skipSpan)
		if err != nil {
			return false, err
		}
		if ok {
			if uncompress {
				_, _, err := e.UncompressedPathFromArtifact(ctx, target, art)
				if err != nil {
					return false, err
				}
			}
		}
	}

	return true, nil
}

func (e *LocalCacheState) UncompressedReaderFromArtifact(ctx context.Context, artifact artifacts.Artifact, target graph.Targeter) (io.ReadCloser, ArtifactManifest, error) {
	stats, _, err := e.ArtifactManifest(ctx, target, artifact)
	if err != nil {
		return nil, ArtifactManifest{}, err
	}

	dir, err := e.cacheDir(target)
	if err != nil {
		return nil, ArtifactManifest{}, err
	}

	r, err := artifacts.UncompressedReaderFromArtifact(artifact, dir.Abs())
	if err != nil {
		return nil, stats, err
	}

	return r, stats, nil
}

func (e *LocalCacheState) UncompressedPathFromArtifact(ctx context.Context, target graph.Targeter, artifact artifacts.Artifact) (string, ArtifactManifest, error) {
	stats, _, err := e.ArtifactManifest(ctx, target, artifact)
	if err != nil {
		return "", ArtifactManifest{}, err
	}

	dir, err := e.cacheDir(target)
	if err != nil {
		return "", ArtifactManifest{}, err
	}

	p, err := UncompressedPathFromArtifact(ctx, target, artifact, dir.Abs(), stats.Size)
	if err != nil {
		return "", stats, err
	}

	return p, stats, err
}

func (e *LocalCacheState) LatestCacheDirExists(target specs.Specer) bool {
	return xfs.PathExists(e.cacheDirForHash(target, LatestDir).Abs())
}

func (e *LocalCacheState) LatestUncompressedPathFromArtifact(ctx context.Context, target graph.Targeter, artifact artifacts.Artifact) (string, ArtifactManifest, error) {
	stats, _ := e.LatestArtifactManifest(ctx, target, artifact)

	p, err := UncompressedPathFromArtifact(ctx, target, artifact, e.cacheDirForHash(target, LatestDir).Abs(), stats.Size)
	if err != nil {
		return "", stats, err
	}

	return p, stats, err
}

func (e *LocalCacheState) tarListPath(artifact artifacts.Artifact, target graph.Targeter) (string, error) {
	dir, err := e.cacheDir(target)
	if err != nil {
		return "", err
	}
	return dir.Join(artifact.Name() + ".list").Abs(), nil
}

type OutDirMeta struct {
	Version int
	Outputs []string
	CacheRW bool
}

func (expectedMeta OutDirMeta) decide(currentMeta OutDirMeta) (expand bool, cleanExpand bool) {
	if !ads.ContainsAll(currentMeta.Outputs, expectedMeta.Outputs) {
		expand = true
	}

	if currentMeta.Version != expectedMeta.Version {
		cleanExpand = true
	} else if currentMeta.CacheRW != expectedMeta.CacheRW {
		cleanExpand = true
	}

	if cleanExpand {
		expand = true
	}

	return expand, cleanExpand
}

func (e *LocalCacheState) Expand(ctx context.Context, ttarget graph.Targeter, outputs []string) (xfs.Path, error) {
	target := ttarget.GraphTarget()

	doneMarker, err := e.ArtifactExists(ctx, target, target.Artifacts.InputHash)
	if err != nil {
		return xfs.Path{}, err
	}

	if !doneMarker {
		return xfs.Path{}, nil
	}

	ltarget := e.Metas.Find(target)
	err = ltarget.expandLock.Lock(ctx)
	if err != nil {
		return xfs.Path{}, err
	}

	defer func() {
		err := ltarget.expandLock.Unlock()
		if err != nil {
			log.Error("unlock %v", err)
		}
	}()

	cacheDir, err := e.cacheDir(target)
	if err != nil {
		return xfs.Path{}, err
	}

	outDir := cacheDir.Join("_output")

	// Legacy...
	_ = os.Remove(cacheDir.Join("_output_hash").Abs())

	outDirMetaPath := cacheDir.Join("_output_meta").Abs()

	expectedMeta := OutDirMeta{
		Version: 1,
		Outputs: outputs,
		CacheRW: e.CacheRW,
	}

	shouldExpand := false
	shouldCleanExpand := false
	if !xfs.PathExists(outDir.Abs()) {
		shouldExpand = true
	} else {
		b, err := os.ReadFile(outDirMetaPath)
		if err != nil && !errors.Is(err, fs.ErrNotExist) {
			return outDir, fmt.Errorf("get outdir meta: %w", err)
		}

		var currentMeta OutDirMeta
		currentMeta.CacheRW = true // Legacy behavior
		_ = json.Unmarshal(b, &currentMeta)

		shouldExpand, shouldCleanExpand = expectedMeta.decide(currentMeta)
	}

	if len(outputs) == 0 {
		shouldExpand = false
	}

	if shouldExpand {
		xfs.MakeDirsReadWrite(outDir.Abs())

		status.Emit(ctx, tgt.TargetStatus(target, "Expanding cache..."))
		if shouldCleanExpand {
			err = os.RemoveAll(outDir.Abs())
			if err != nil {
				return outDir, err
			}
		}

		err = os.MkdirAll(outDir.Abs(), os.ModePerm)
		if err != nil {
			return outDir, err
		}

		untarDedup := sets.NewStringSet(0)

		for _, name := range outputs {
			artifact := target.Artifacts.OutTar(name)

			manifest, _, err := e.ArtifactManifest(ctx, target, artifact)
			if err != nil {
				return xfs.Path{}, err
			}

			r, err := artifacts.UncompressedReaderFromArtifact(artifact, cacheDir.Abs())
			if err != nil {
				return outDir, err
			}
			defer r.Close()

			var progress func(written int64)
			if manifest.Size > 0 {
				progress = func(written int64) {
					percent := math.Round(xmath.Percent(written, manifest.Size))

					status.EmitInteractive(ctx, tgt.TargetOutputStatus(target, artifact.Name(), xmath.FormatPercent("Expanding cache [P]...", percent)))
				}
			}

			tarPath, err := e.tarListPath(target.Artifacts.OutTar(name), target)
			if err != nil {
				return xfs.Path{}, err
			}

			err = tar.UntarContext(ctx, r, outDir.Abs(), tar.UntarOptions{
				ListPath: tarPath,
				Dedup:    untarDedup,
				Progress: progress,
			})
			if err != nil {
				return outDir, fmt.Errorf("%v: untar: %w", name, err)
			}

			_ = r.Close()
		}

		b, err := json.Marshal(expectedMeta)
		if err != nil {
			return xfs.Path{}, err
		}

		err = os.WriteFile(outDirMetaPath, b, os.ModePerm)
		if err != nil {
			return outDir, fmt.Errorf("write outdir meta: %w", err)
		}

		if !e.CacheRW {
			xfs.MakeDirsReadOnly(outDir.Abs())
		}
	}

	e.Metas.Find(target).outExpansionRoot = outDir

	return outDir, nil
}

type ActualFileCollector interface {
	PopulateActualFiles(ctx context.Context, t *Target, outputs []string) error
}

type TargetOpts struct {
	ActualFilesCollector        ActualFileCollector
	ActualFilesCollectorOutputs []string
}

func (e *LocalCacheState) Target(ctx context.Context, target graph.Targeter, o TargetOpts) (*Target, error) {
	t := e.Metas.Find(target)

	if o.ActualFilesCollector != nil {
		// TODO: make a bit smarter so that it doesnt collect them again if the request outputs is already done

		if len(o.ActualFilesCollectorOutputs) > 0 || target.Spec().HasSupportFiles {
			status.Emit(ctx, tgt.TargetStatus(target, "Hydrating output..."))
		}

		ctx, span := e.Observability.SpanCollectOutput(ctx, target.GraphTarget())
		err := observability.DoE(span, func() error {
			return o.ActualFilesCollector.PopulateActualFiles(ctx, t, o.ActualFilesCollectorOutputs)
		})
		if err != nil {
			return nil, err
		}
	}

	return t, nil
}

func (e *LocalCacheState) CleanTarget(target specs.Specer, async bool) error {
	cacheDir := e.cacheDirForHash(target, "").Abs()

	xfs.MakeDirsReadWrite(cacheDir)

	err := xfs.DeleteDir(cacheDir, async)
	if err != nil {
		return err
	}

	return nil
}

func (e *LocalCacheState) VFSLocation(target graph.Targeter) (vfs.Location, error) {
	dir, err := e.cacheDir(target)
	if err != nil {
		return nil, err
	}

	rel, err := filepath.Rel(e.Path.Abs(), dir.Abs())
	if err != nil {
		return nil, err
	}

	return e.Location.NewLocation(rel + "/")
}

func (e *LocalCacheState) RegisterRemove(target graph.Targeter) {
	dir, err := e.cacheDir(target)
	if err == nil {
		return
	}
	e.Finalizers.RegisterRemove(dir.Abs())
}

func (e *LocalCacheState) ArtifactExists(ctx context.Context, target graph.Targeter, artifact artifacts.Artifact) (bool, error) {
	root, err := e.cacheDir(target)
	if err != nil {
		return false, err
	}

	for _, name := range []string{artifact.GzFileName(), artifact.FileName()} {
		if xfs.PathExists(root.Join(name).Abs()) {
			return true, nil
		}
	}

	return false, nil
}

func (e *LocalCacheState) Post(ctx context.Context, target *graph.Target, outputs []string) error {
	ltarget := e.Metas.Find(target)

	hash, err := e.HashInput(target)
	if err != nil {
		return err
	}

	_, err = e.Expand(ctx, target, outputs)
	if err != nil {
		return fmt.Errorf("expand: %w", err)
	}

	err = e.codegenLink(ctx, ltarget)
	if err != nil {
		return fmt.Errorf("codegenlink: %w", err)
	}

	err = e.LinkLatestCache(target, hash)
	if err != nil {
		return fmt.Errorf("linklatest: %w", err)
	}

	if target.Cache.Enabled && e.EnableGC {
		status.Emit(ctx, tgt.TargetStatus(target, "GC..."))

		err := e.GCTargets([]*graph.Target{target}, nil, false)
		if err != nil {
			log.Errorf("gc %v: %v", target.Addr, err)
		}
	}

	return nil
}
