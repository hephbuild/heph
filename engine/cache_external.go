package engine

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/worker"
)

func (e *Engine) scheduleStoreExternalCache(ctx context.Context, target *Target, cache graph.CacheConfig) *worker.Job {
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

func (e *Engine) scheduleStoreExternalCacheArtifact(ctx context.Context, target *Target, cache graph.CacheConfig, artifact artifacts.Artifact, deps *worker.WaitGroup) *worker.Job {
	return e.Pool.Schedule(ctx, &worker.Job{
		Name: fmt.Sprintf("cache %v %v %v", target.FQN, cache.Name, artifact.Name()),
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			err := e.RemoteCache.StoreArtifact(ctx, target, cache, artifact)
			if err != nil {
				return fmt.Errorf("store remote cache %v: %v %w", cache.Name, target.FQN, err)
			}

			return nil
		},
	})
}
