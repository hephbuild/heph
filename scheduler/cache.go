package scheduler

import (
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/lcache"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/rcache"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/worker"
	"os"
	"sync"
)

func (e *Scheduler) pullOrGetCacheAndPost(ctx context.Context, target *graph.Target, outputs []string, followHint, uncompress bool) (bool, error) {
	_, cached, err := e.pullOrGetCache(ctx, target, outputs, false, false, followHint, uncompress)
	if err != nil {
		return false, fmt.Errorf("pullorget: %w", err)
	}

	if !cached {
		return false, nil
	}

	_, err = e.LocalCache.Target(ctx, target, lcache.TargetOpts{
		ActualFilesCollector:        e.LocalCache,
		ActualFilesCollectorOutputs: outputs,
	})
	if err != nil {
		return false, err
	}

	err = e.LocalCache.Post(ctx, target, outputs)
	if err != nil {
		return false, fmt.Errorf("postrunwarm: %w", err)
	}

	return true, nil
}

func (e *Scheduler) pullOrGetCache(ctx context.Context, target *graph.Target, outputs []string, onlyMeta, onlyMetaLocal, followHint, uncompress bool) (rpulled, rcached bool, rerr error) {
	status.Emit(ctx, tgt.TargetStatus(target, "Checking local cache..."))

	// We may want to check that the tar.gz data is available locally, if not it will make sure you can acquire it from cache
	cached, err := e.LocalCache.GetLocalCache(ctx, target, outputs, onlyMetaLocal, false, uncompress)
	if err != nil {
		return false, false, fmt.Errorf("getlocal: %w", err)
	}

	if cached {
		return false, true, nil
	}

	orderedCaches, err := e.RemoteCache.OrderedCaches(ctx)
	if err != nil {
		return false, false, fmt.Errorf("orderedcaches: %w", err)
	}

	var statusOnce sync.Once

	for _, cache := range orderedCaches {
		if !cache.Read {
			continue
		}

		if !target.Cache.NamedEnabled(cache.Name) {
			continue
		}

		if followHint && e.RemoteCache.Hints.Get(target.Addr, cache.Name).Skip() {
			continue
		}

		statusOnce.Do(func() {
			status.Emit(ctx, tgt.TargetStatus(target, "Checking remote caches..."))
		})

		externalCached, err := e.pullExternalCache(ctx, target, outputs, onlyMeta, cache)
		if err != nil {
			log.Warnf("%v: %v", cache.Name, err)
			continue
		}

		if externalCached {
			cached, err := e.LocalCache.GetLocalCache(ctx, target, outputs, onlyMeta, true, uncompress)
			if err != nil {
				log.Errorf("local: %v", err)
				continue
			}

			if cached {
				return true, true, nil
			}

			log.Warnf("%v cache %v: local cache is supposed to exist locally, but failed getLocalCache, this is not supposed to happen", target.Addr, cache.Name)
		} else {
			if e.Config.Engine.CacheHints {
				children, err := e.Graph.DAG().GetDescendants(target.Target)
				if err != nil {
					log.Error(fmt.Errorf("descendants: %w", err))
				}

				for _, child := range children {
					e.RemoteCache.Hints.Set(child.Addr, cache.Name, rcache.HintSkip{})
				}
			}
		}
	}

	return false, false, nil
}

func (e *Scheduler) pullExternalCache(ctx context.Context, target *graph.Target, outputs []string, onlyMeta bool, cache graph.CacheConfig) (_ bool, rerr error) {
	ctx, span := e.Observability.SpanExternalCacheGet(ctx, target.GraphTarget(), cache.Name, outputs, onlyMeta)
	defer rcache.SpanEndIgnoreNotExist(span, rerr)

	err := e.RemoteCache.DownloadArtifact(ctx, target, cache, target.Artifacts.InputHash)
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
			exists, err := e.RemoteCache.ArtifactExists(ctx, cache, target, tarArtifact)
			if err != nil {
				return false, err
			}

			if !exists {
				span.SetCacheHit(false)
				return false, nil
			}
		} else {
			err := e.RemoteCache.DownloadArtifact(ctx, target, cache, tarArtifact)
			if err != nil {
				return false, err
			}
		}

		err = e.RemoteCache.DownloadArtifact(ctx, target, cache, target.Artifacts.OutHash(output))
		if err != nil {
			return false, err
		}
	}

	span.SetCacheHit(true)

	return true, nil
}

func (e *Scheduler) scheduleStoreExternalCache(ctx context.Context, target *graph.Target, cache graph.CacheConfig) *worker.Job {
	// input hash is used as a marker that everything went well,
	// wait for everything else to be done before copying the input hash
	inputHashArtifact := target.Artifacts.InputHash

	deps := &worker.WaitGroup{}
	for _, artifact := range target.Artifacts.All() {
		if artifact.Name() == inputHashArtifact.Name() {
			continue
		}

		j := e.scheduleStoreExternalCacheArtifact(ctx, target, cache, artifact, nil)
		deps.Add(j)
	}

	return e.scheduleStoreExternalCacheArtifact(ctx, target, cache, inputHashArtifact, deps)
}

func (e *Scheduler) scheduleStoreExternalCacheArtifact(ctx context.Context, target *graph.Target, cache graph.CacheConfig, artifact artifacts.Artifact, deps *worker.WaitGroup) *worker.Job {
	return e.Pool.Schedule(ctx, &worker.Job{
		Name: fmt.Sprintf("cache %v %v %v", target.Addr, cache.Name, artifact.Name()),
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			exists, err := e.LocalCache.ArtifactExists(ctx, target, artifact)
			if err != nil {
				return err
			}

			if !exists {
				if !artifact.GenRequired() {
					return nil
				}

				return fmt.Errorf("%v: %v is supposed to exist but doesn't", target.Addr, artifact.Name())
			}

			err = e.RemoteCache.StoreArtifact(ctx, target, cache, artifact)
			if err != nil {
				return fmt.Errorf("store remote cache %v: %v %w", cache.Name, target.Addr, err)
			}

			return nil
		},
	})
}
