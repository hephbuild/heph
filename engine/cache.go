package engine

import (
	"context"
	"errors"
	"fmt"
	"heph/engine/artifacts"
	"heph/engine/observability"
	log "heph/hlog"
	"heph/rcache"
	"heph/utils/fs"
	"heph/utils/instance"
	"os"
)

func (e *Engine) cacheDir(target *Target) fs.Path {
	return e.cacheDirForHash(target, e.hashInput(target))
}

func (e *Engine) cacheDirForHash(target *Target, inputHash string) fs.Path {
	// TODO: cache
	folder := "__target_" + target.Name
	if !target.Cache.Enabled {
		folder = "__target_tmp_" + instance.UID + "_" + target.Name
	}
	return e.HomeDir.Join("cache", target.Package.FullName, folder, inputHash)
}

func (e *Engine) linkLatestCache(target *Target, from string) error {
	latestDir := e.cacheDirForHash(target, "latest")

	err := os.RemoveAll(latestDir.Abs())
	if err != nil {
		return err
	}

	err = os.Symlink(from, latestDir.Abs())
	if err != nil && !errors.Is(err, os.ErrExist) {
		return err
	}

	return nil
}

func (e *TargetRunEngine) pullOrGetCacheAndPost(ctx context.Context, target *Target, outputs []string, followHint bool) (bool, error) {
	pulled, cached, err := e.pullOrGetCache(ctx, target, outputs, false, false, followHint)
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

func (e *TargetRunEngine) pullOrGetCache(ctx context.Context, target *Target, outputs []string, onlyMeta, onlyMetaLocal, followHint bool) (rpulled bool, rcached bool, rerr error) {
	e.Status(TargetStatus(target, "Checking local cache..."))

	// We may want to check that the tar.gz data is available locally, if not it will make sure you can acquire it from cache
	cached, err := e.getLocalCache(ctx, target, outputs, onlyMetaLocal, false)
	if err != nil {
		return false, false, fmt.Errorf("getlocal: %w", err)
	}

	if cached {
		return false, true, nil
	}

	e.Status(TargetStatus(target, "Checking remote caches..."))

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
			cached, err := e.getLocalCache(ctx, target, outputs, onlyMeta, true)
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
				children, err := e.DAG().GetDescendants(target)
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

func (e *TargetRunEngine) pullExternalCache(ctx context.Context, target *Target, outputs []string, onlyMeta bool, cache CacheConfig) (_ bool, rerr error) {
	ctx, span := e.Observability.SpanExternalCacheGet(ctx, target.Target, cache.Name, outputs, onlyMeta)
	defer span.EndError(rerr)

	err := e.downloadExternalCache(ctx, target, cache, target.artifacts.InputHash)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return false, nil
		}
		return false, err
	}

	for _, output := range outputs {
		tarArtifact := target.artifacts.OutTar(output)
		if onlyMeta {
			exists, err := e.existsExternalCache(ctx, target, cache, tarArtifact)
			if err != nil {
				return false, err
			}

			if !exists {
				return false, nil
			}
		} else {
			err := e.downloadExternalCache(ctx, target, cache, tarArtifact)
			if err != nil {
				return false, err
			}
		}

		err = e.downloadExternalCache(ctx, target, cache, target.artifacts.OutHash(output))
		if err != nil {
			return false, err
		}
	}

	return true, nil
}

func (e *Engine) getLocalCacheArtifact(ctx context.Context, target *Target, artifact artifacts.Artifact, isAfterPulling bool) bool {
	setCacheHit := func() {}
	if !isAfterPulling {
		var span *observability.TargetArtifactCacheSpan
		ctx, span = e.Observability.SpanLocalCacheCheck(ctx, target.Target, artifact)
		defer span.End()
		setCacheHit = func() {
			span.SetCacheHit(true)
		}
	}

	cacheDir := e.cacheDir(target)

	p := cacheDir.Join(artifact.Name()).Abs()
	if !fs.PathExists(p) {
		return false
	}

	setCacheHit()
	return true
}

func (e *Engine) getLocalCache(ctx context.Context, target *Target, outputs []string, onlyMeta, isAfterPulling bool) (bool, error) {
	if !e.getLocalCacheArtifact(ctx, target, target.artifacts.InputHash, isAfterPulling) {
		return false, nil
	}

	for _, output := range outputs {
		if !e.getLocalCacheArtifact(ctx, target, target.artifacts.OutHash(output), isAfterPulling) {
			return false, nil
		}

		if !onlyMeta {
			if !e.getLocalCacheArtifact(ctx, target, target.artifacts.OutTar(output), isAfterPulling) {
				return false, nil
			}
		}
	}

	return true, nil
}
