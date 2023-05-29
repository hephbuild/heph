package engine

import (
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/observability"
	"github.com/hephbuild/heph/rcache"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/utils/xfs"
	"os"
)

// Deprecated: use LocalCache.cacheDir
func (e *Engine) cacheDir(target *Target) xfs.Path {
	return e.LocalCache.cacheDir(target)
}

// Deprecated: use LocalCache.cacheDirForHash
func (e *Engine) cacheDirForHash(target *Target, inputHash string) xfs.Path {
	return e.LocalCache.cacheDirForHash(target, inputHash)
}

func (e *Engine) pullOrGetCacheAndPost(ctx context.Context, target *Target, outputs []string, followHint, uncompress bool) (bool, error) {
	pulled, cached, err := e.pullOrGetCache(ctx, target, outputs, false, false, followHint, uncompress)
	if err != nil {
		return false, fmt.Errorf("pullorget: %w", err)
	}

	if !cached {
		return false, nil
	}

	err = e.postRunOrWarm(ctx, target, outputs, pulled)
	if err != nil {
		return false, fmt.Errorf("postrunwarm: %w", err)
	}

	return true, nil
}

func (e *Engine) pullOrGetCache(ctx context.Context, target *Target, outputs []string, onlyMeta, onlyMetaLocal, followHint, uncompress bool) (rpulled, rcached bool, rerr error) {
	status.Emit(ctx, TargetStatus(target, "Checking local cache..."))

	// We may want to check that the tar.gz data is available locally, if not it will make sure you can acquire it from cache
	cached, err := e.getLocalCache(ctx, target, outputs, onlyMetaLocal, false, uncompress)
	if err != nil {
		return false, false, fmt.Errorf("getlocal: %w", err)
	}

	if cached {
		return false, true, nil
	}

	status.Emit(ctx, TargetStatus(target, "Checking remote caches..."))

	orderedCaches, err := e.OrderedCaches(ctx)
	if err != nil {
		return false, false, fmt.Errorf("orderedcaches: %w", err)
	}

	for _, cache := range orderedCaches {
		if !cache.Read {
			continue
		}

		if !target.Cache.NamedEnabled(cache.Name) {
			continue
		}

		if followHint && e.RemoteCacheHints.Get(target.FQN, cache.Name).Skip() {
			continue
		}

		externalCached, err := e.pullExternalCache(ctx, target, outputs, onlyMeta, cache)
		if err != nil {
			log.Warnf("%v: %v", cache.Name, err)
			continue
		}

		if externalCached {
			cached, err := e.getLocalCache(ctx, target, outputs, onlyMeta, true, uncompress)
			if err != nil {
				log.Errorf("local: %v", err)
				continue
			}

			if cached {
				return true, true, nil
			}

			log.Warnf("%v cache %v: local cache is supposed to exist locally, but failed getLocalCache, this is not supposed to happen", target.FQN, cache.Name)
		} else {
			if e.Config.Engine.CacheHints {
				children, err := e.Graph.DAG().GetDescendants(target.Target)
				if err != nil {
					log.Error(fmt.Errorf("descendants: %w", err))
				}

				for _, child := range children {
					e.RemoteCacheHints.Set(child.FQN, cache.Name, rcache.HintSkip{})
				}
			}
		}
	}

	return false, false, nil
}

func (e *Engine) pullExternalCache(ctx context.Context, target *Target, outputs []string, onlyMeta bool, cache graph.CacheConfig) (_ bool, rerr error) {
	ctx, span := e.Observability.SpanExternalCacheGet(ctx, target.Target.Target, cache.Name, outputs, onlyMeta)
	defer span.EndError(rerr)

	err := e.downloadExternalCache(ctx, target, cache, target.Artifacts.InputHash)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			span.SetCacheHit(false)
			return false, nil
		}
		return false, err
	}

	for _, output := range outputs {
		tarArtifact := target.Artifacts.OutTar(output)
		if onlyMeta {
			exists, err := e.existsExternalCache(ctx, target, cache, tarArtifact)
			if err != nil {
				return false, err
			}

			if !exists {
				span.SetCacheHit(false)
				return false, nil
			}
		} else {
			err := e.downloadExternalCache(ctx, target, cache, tarArtifact)
			if err != nil {
				return false, err
			}
		}

		err = e.downloadExternalCache(ctx, target, cache, target.Artifacts.OutHash(output))
		if err != nil {
			return false, err
		}
	}

	span.SetCacheHit(true)

	return true, nil
}

func (e *Engine) getLocalCacheArtifact(ctx context.Context, target *Target, artifact artifacts.Artifact, skipSpan bool) bool {
	setCacheHit := func(bool) {}
	if !skipSpan {
		var span *observability.TargetArtifactCacheSpan
		ctx, span = e.Observability.SpanLocalCacheCheck(ctx, target.Target.Target, artifact)
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
			return true
		}
	}

	setCacheHit(false)
	return false
}

func (e *Engine) getLocalCache(ctx context.Context, target *Target, outputs []string, onlyMeta, skipSpan, uncompress bool) (bool, error) {
	if !e.getLocalCacheArtifact(ctx, target, target.Artifacts.InputHash, skipSpan) {
		return false, nil
	}

	for _, output := range outputs {
		if !e.getLocalCacheArtifact(ctx, target, target.Artifacts.OutHash(output), skipSpan) {
			return false, nil
		}

		if !onlyMeta {
			art := target.Artifacts.OutTar(output)

			if !e.getLocalCacheArtifact(ctx, target, art, skipSpan) {
				return false, nil
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
