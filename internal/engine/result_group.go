package engine

import (
	"context"
	"fmt"
	"time"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/lib/tref"
)

func (e *Engine) resultGroup(ctx context.Context, rs *RequestState, def *LightLinkedTarget, hashin string) (*ExecuteResultLocks, error) {
	results, err := e.depsResults(ctx, rs, def)
	if err != nil {
		return nil, fmt.Errorf("deps results: %w", err)
	}
	defer results.Unlock(ctx)

	manifest := &hartifact.Manifest{
		Version:   "v1",
		Target:    tref.Format(def.GetRef()),
		CreatedAt: time.Now(),
		Hashin:    hashin,
	}

	var artifacts []*ResultArtifact
	var locks CacheLocks
	for _, result := range results {
		for _, artifact := range result.Artifacts {
			partifact := artifactGroupMap{Artifact: artifact, group: ""} // TODO support output group

			martifact, err := hartifact.ProtoArtifactToManifest(artifact.GetHashout(), partifact)
			if err != nil {
				return nil, fmt.Errorf("proto artifact to manifest: %w", err)
			}

			artifacts = append(artifacts, &ResultArtifact{
				Artifact: partifact,
				Manifest: martifact,
			})

			manifest.Artifacts = append(manifest.Artifacts, martifact)

			locks.AddFrom(result.Locks)
		}
	}

	err = locks.RLock(ctx)
	if err != nil {
		return nil, err
	}

	return &ExecuteResultLocks{
		Result: Result{
			Def:       def,
			Hashin:    hashin,
			Artifacts: artifacts,
			Manifest:  manifest,
		}.Sorted(),
		Locks: &locks,
	}, nil
}
