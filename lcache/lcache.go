package lcache

import (
	"context"
	"errors"
	"fmt"
	"github.com/c2fo/vfs/v6"
	vfsos "github.com/c2fo/vfs/v6/backend/os"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/observability"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/finalizers"
	"github.com/hephbuild/heph/utils/locks"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/tar"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/vfssimple"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"time"
)

type targetCacheKey struct {
	fqn  string
	hash string
}

func (k targetCacheKey) String() string {
	return k.fqn + "_" + k.hash
}

type targetOutCacheKey struct {
	fqn    string
	output string
	hash   string
}

func (k targetOutCacheKey) String() string {
	return k.fqn + "|" + k.output + "_" + k.hash
}

type LocalCacheState struct {
	Location      *vfsos.Location
	Path          xfs.Path
	Targets       *graph.Targets
	TargetMetas   *TargetMetas
	Root          *hroot.State
	Graph         *graph.State
	Observability *observability.Observability
	Finalizers    *finalizers.Finalizers

	cacheHashInputTargetMutex  maps.KMutex
	cacheHashInput             *maps.Map[targetCacheKey, string]
	cacheHashOutputTargetMutex maps.KMutex
	cacheHashOutput            *maps.Map[targetOutCacheKey, string] // TODO: LRU
	cacheHashInputPathsModtime *maps.Map[targetCacheKey, map[string]time.Time]
}

func NewState(root *hroot.State, g *graph.State, obs *observability.Observability, finalizers *finalizers.Finalizers) (*LocalCacheState, error) {
	cachePath := root.Home.Join("cache")
	loc, err := vfssimple.NewLocation("file://" + cachePath.Abs() + "/")
	if err != nil {
		return nil, fmt.Errorf("lcache location: %w", err)
	}

	s := &LocalCacheState{
		Location:      loc.(*vfsos.Location),
		Path:          cachePath,
		Targets:       g.Targets(),
		Root:          root,
		Graph:         g,
		Observability: obs,
		Finalizers:    finalizers,
		TargetMetas: NewTargetMetas(func(fqn string) *Target {
			gtarget := g.Targets().Find(fqn)

			t := &Target{
				Target:     gtarget,
				cacheLocks: map[string]locks.Locker{},
			}

			t.cacheLocks = make(map[string]locks.Locker, len(t.Artifacts.All()))
			for _, artifact := range t.Artifacts.All() {
				ts := t.Spec()
				resource := artifact.Name()

				p := lockPath(root, t, "cache_"+resource)

				l := locks.NewFlock(ts.FQN+" ("+resource+")", p)

				t.cacheLocks[artifact.Name()] = l
			}

			return t
		}),
		cacheHashInputTargetMutex:  maps.KMutex{},
		cacheHashInput:             &maps.Map[targetCacheKey, string]{},
		cacheHashOutputTargetMutex: maps.KMutex{},
		cacheHashOutput:            &maps.Map[targetOutCacheKey, string]{},
		cacheHashInputPathsModtime: &maps.Map[targetCacheKey, map[string]time.Time]{},
	}

	return s, nil
}

func (e *LocalCacheState) StoreCache(ctx context.Context, ttarget graph.Targeter, allArtifacts []ArtifactWithProducer, compress bool) (rerr error) {
	target := ttarget.GraphTarget()

	if target.ConcurrentExecution {
		log.Debugf("%v concurrent execution, skipping storeCache", target.FQN)
		return nil
	}

	if target.Cache.Enabled {
		status.Emit(ctx, tgt.TargetStatus(target, "Caching..."))
	} else if len(target.Artifacts.Out) > 0 {
		status.Emit(ctx, tgt.TargetStatus(target, "Storing output..."))
	}

	ctx, span := e.Observability.SpanLocalCacheStore(ctx, target.Target)
	defer span.EndError(rerr)

	hash, err := e.HashInput(target)
	if err != nil {
		return err
	}
	dir := e.cacheDir(target).Abs()

	err = os.RemoveAll(dir)
	if err != nil {
		return err
	}

	err = os.MkdirAll(dir, os.ModePerm)
	if err != nil {
		return err
	}

	err = e.GenArtifacts(ctx, dir, target, allArtifacts, compress)
	if err != nil {
		return err
	}

	err = xfs.CreateParentDir(dir)
	if err != nil {
		return err
	}

	return e.LinkLatestCache(target, hash)
}

func (e *LocalCacheState) LinkLatestCache(target targetspec.Specer, hash string) error {
	latestDir := e.cacheDirForHash(target, "latest").Abs()
	fromDir := e.cacheDirForHash(target, hash).Abs()

	err := os.RemoveAll(latestDir)
	if err != nil {
		return err
	}

	err = os.Symlink(fromDir, latestDir)
	if err != nil && !errors.Is(err, os.ErrExist) {
		return err
	}

	return nil
}

func (e *LocalCacheState) ResetCacheHashInput(spec targetspec.Specer) {
	target := spec.Spec()

	e.cacheHashInput.DeleteP(func(k targetCacheKey) bool {
		return k.fqn == target.FQN
	})

	e.cacheHashInputPathsModtime.DeleteP(func(k targetCacheKey) bool {
		return k.fqn == target.FQN
	})
}

func (e *LocalCacheState) HasArtifact(ctx context.Context, target graph.Targeter, artifact artifacts.Artifact, skipSpan bool) (bool, error) {
	unlock, err := e.LockArtifact(ctx, target, artifact)
	if err != nil {
		return false, err
	}
	defer unlock()

	setCacheHit := func(bool) {}
	if !skipSpan {
		var span *observability.TargetArtifactCacheSpan
		ctx, span = e.Observability.SpanLocalCacheCheck(ctx, target.TGTTarget(), artifact)
		defer span.End()
		setCacheHit = func(v bool) {
			span.SetCacheHit(v)
		}
	}

	cacheDir := e.cacheDir(target)

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

func (e *LocalCacheState) GetLocalCache(ctx context.Context, ttarget graph.Targeter, outputs []string, onlyMeta, skipSpan, uncompress bool) (bool, error) {
	target := ttarget.GraphTarget()

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
				_, err := UncompressedPathFromArtifact(ctx, target, art, e.cacheDir(target).Abs())
				if err != nil {
					return false, err
				}
			}
		}
	}

	return true, nil
}

func (e *LocalCacheState) UncompressedReaderFromArtifact(artifact artifacts.Artifact, target graph.Targeter) (io.ReadCloser, error) {
	return artifacts.UncompressedReaderFromArtifact(artifact, e.cacheDir(target).Abs())
}

func (e *LocalCacheState) UncompressedPathFromArtifact(ctx context.Context, target graph.Targeter, artifact artifacts.Artifact) (string, error) {
	return UncompressedPathFromArtifact(ctx, target, artifact, e.cacheDir(target).Abs())
}

func (e *LocalCacheState) tarListPath(artifact artifacts.Artifact, target graph.Targeter) string {
	return e.cacheDir(target).Join(artifact.Name() + ".list").Abs()
}

func (e *LocalCacheState) Expand(ctx context.Context, ttarget graph.Targeter, outputs []string) (xfs.Path, error) {
	target := ttarget.GraphTarget()

	cacheDir := e.cacheDir(target)

	doneMarker := cacheDir.Join(target.Artifacts.InputHash.FileName()).Abs()
	if !xfs.PathExists(doneMarker) {
		return xfs.Path{}, nil
	}

	outDir := e.cacheDir(target).Join("_output")
	outDirHashPath := e.cacheDir(target).Join("_output_hash").Abs()

	// TODO: This can be a problem, where 2 targets depends on the same target, but with different outputs,
	// leading to the expand overriding each other

	outDirHash := "2|" + strings.Join(outputs, ",")

	shouldExpand := false
	if !xfs.PathExists(outDir.Abs()) {
		shouldExpand = true
	} else {
		b, err := os.ReadFile(outDirHashPath)
		if err != nil && !errors.Is(err, fs.ErrNotExist) {
			return outDir, fmt.Errorf("outdirhash: %w", err)
		}

		if len(b) > 0 && strings.TrimSpace(string(b)) != outDirHash {
			shouldExpand = true
		}
	}

	if len(outputs) == 0 {
		shouldExpand = false
	}

	if shouldExpand {
		status.Emit(ctx, tgt.TargetStatus(target, "Expanding cache..."))
		tmpOutDir := e.cacheDir(target).Join("_output_tmp").Abs()

		err := os.RemoveAll(tmpOutDir)
		if err != nil {
			return outDir, err
		}

		err = os.MkdirAll(tmpOutDir, os.ModePerm)
		if err != nil {
			return outDir, err
		}

		untarDedup := sets.NewStringSet(0)

		for _, name := range outputs {
			r, err := artifacts.UncompressedReaderFromArtifact(target.Artifacts.OutTar(name), cacheDir.Abs())
			if err != nil {
				return outDir, err
			}

			err = tar.UntarContext(ctx, r, tmpOutDir, tar.UntarOptions{
				ListPath: e.tarListPath(target.Artifacts.OutTar(name), target),
				Dedup:    untarDedup,
			})
			_ = r.Close()
			if err != nil {
				return outDir, fmt.Errorf("%v: untar: %w", name, err)
			}
		}

		err = os.RemoveAll(outDir.Abs())
		if err != nil {
			return outDir, err
		}

		err = os.Rename(tmpOutDir, outDir.Abs())
		if err != nil {
			return outDir, err
		}

		err = os.WriteFile(outDirHashPath, []byte(outDirHash), os.ModePerm)
		if err != nil {
			return outDir, fmt.Errorf("outdirhash: %w", err)
		}
	}

	return outDir, nil
}

func (e *LocalCacheState) PathsFromArtifact(ctx context.Context, target graph.Targeter, artifact artifacts.Artifact) (xfs.RelPaths, error) {
	r, err := e.UncompressedReaderFromArtifact(artifact, target)
	if err != nil {
		return nil, err
	}
	defer r.Close()

	files, err := tar.UntarList(ctx, r, e.tarListPath(artifact, target))
	if err != nil {
		return nil, err
	}

	ps := xfs.RelPaths(ads.Map(files, func(path string) xfs.RelPath {
		return xfs.NewRelPath(path)
	}))

	ps.Sort()

	return ps, nil
}

func (e *LocalCacheState) CleanTarget(target targetspec.Specer, async bool) error {
	cacheDir := e.cacheDirForHash(target, "")
	err := xfs.DeleteDir(cacheDir.Abs(), async)
	if err != nil {
		return err
	}

	return nil
}

func (e *LocalCacheState) LatestCacheDir(target targetspec.Specer) xfs.Path {
	return e.cacheDirForHash(target, "latest")
}

func (e *LocalCacheState) VFSLocation(target graph.Targeter) (vfs.Location, error) {
	rel, err := filepath.Rel(e.Path.Abs(), e.cacheDir(target).Abs())
	if err != nil {
		return nil, err
	}

	return e.Location.NewLocation(rel + "/")
}

func (e *LocalCacheState) RegisterRemove(target graph.Targeter) {
	e.Finalizers.RegisterRemove(e.cacheDir(target).Abs())
}

func (e *LocalCacheState) Exists(ctx context.Context, target graph.Targeter, artifact artifacts.Artifact) (bool, error) {
	root := e.cacheDir(target)

	for _, name := range []string{artifact.GzFileName(), artifact.FileName()} {
		if xfs.PathExists(root.Join(name).Abs()) {
			return true, nil
		}
	}

	return false, nil
}
