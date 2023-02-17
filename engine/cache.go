package engine

import (
	"context"
	"errors"
	"fmt"
	"heph/engine/artifacts"
	log "heph/hlog"
	"heph/utils/fs"
	"os"
	"time"
)

func (e *Engine) cacheDir(target *Target) fs.Path {
	return e.cacheDirForHash(target, e.hashInput(target))
}

func genInstanceUid() string {
	host, _ := os.Hostname()
	return fmt.Sprintf("%v%v%v", os.Getpid(), host, time.Now().Nanosecond())
}

var InstanceUID = genInstanceUid()

func (e *Engine) cacheDirForHash(target *Target, inputHash string) fs.Path {
	// TODO: cache
	folder := "__target_" + target.Name
	if !target.Cache.Enabled {
		folder = "__target_tmp_" + InstanceUID + "_" + target.Name
	}
	return e.HomeDir.Join("cache", target.Package.FullName, folder, inputHash)
}

func (e *Engine) cacheOutTarName(name string) string {
	return "out_" + name + ".tar.gz"
}

func (e *Engine) cacheOutHashName(name string) string {
	return "hash_out_" + name
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

func (e *TargetRunEngine) pullOrGetCacheAndPost(ctx context.Context, target *Target, outputs []string) (bool, error) {
	cached, err := e.pullOrGetCache(ctx, target, outputs, false)
	if err != nil {
		return false, err
	}

	if !cached {
		return false, nil
	}

	return true, e.postRunOrWarm(ctx, target, outputs)
}

func (e *TargetRunEngine) pullOrGetCache(ctx context.Context, target *Target, outputs []string, onlyMeta bool) (bool, error) {
	e.Status(TargetStatus(target, "Checking local cache..."))

	cached, err := e.getLocalCache(ctx, target, outputs, onlyMeta)
	if err != nil {
		return false, err
	}

	if cached {
		return true, nil
	}

	for _, cache := range e.Config.Cache {
		if !cache.Read {
			continue
		}

		if !target.Cache.NamedEnabled(cache.Name) {
			continue
		}

		externalCached, err := e.pullExternalCache(ctx, target, outputs, onlyMeta, cache)
		if err != nil {
			log.Errorf("%v: %v", cache.Name, err)
			continue
		}

		if externalCached {
			cached, err := e.getLocalCache(ctx, target, outputs, onlyMeta)
			if err != nil {
				log.Errorf("local: %v", err)
				continue
			}

			if cached {
				return true, err
			}

			log.Warnf("%v: local cache is supposed to be pulled locally, but failed getLocalCache, this is not supposed to happen", cache.Name)
		}
	}

	return false, nil
}

func (e *TargetRunEngine) pullExternalCache(ctx context.Context, target *Target, outputs []string, onlyMeta bool, cache CacheConfig) (bool, error) {
	err := e.downloadExternalCache(ctx, target, cache, target.artifacts.InputHash)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return false, nil
		}
		return false, err
	}

	for _, output := range outputs {
		if !onlyMeta {
			err := e.downloadExternalCache(ctx, target, cache, target.artifacts.OutTar(output))
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

func (e *Engine) getLocalCacheArtifact(ctx context.Context, target *Target, artifact artifacts.Artifact) bool {
	cacheDir := e.cacheDir(target)

	p := cacheDir.Join(artifact.Name()).Abs()
	if !fs.PathExists(p) {
		return false
	}

	return true
}

func (e *Engine) getLocalCache(ctx context.Context, target *Target, outputs []string, onlyMeta bool) (bool, error) {
	cacheDir := e.cacheDir(target)

	if !e.getLocalCacheArtifact(ctx, target, target.artifacts.InputHash) {
		return false, nil
	}

	for _, output := range outputs {
		if !e.getLocalCacheArtifact(ctx, target, target.artifacts.OutHash(output)) {
			return false, nil
		}

		if !onlyMeta {
			if !e.getLocalCacheArtifact(ctx, target, target.artifacts.OutTar(output)) {
				return false, nil
			}
		}
	}

	if !onlyMeta {
		err := e.linkLatestCache(target, cacheDir.Abs())
		if err != nil {
			return false, nil
		}
	}

	return true, nil
}
