package engine

import (
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/lcache"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/mds"
)

type artifactProducer struct {
	artifacts.Artifact
	lcache.ArtifactProducer
}

// orderedArtifactProducers returns artifact in hashing order
// For hashing to work properly, support_files tar must go first, then support_files hash
// then the other artifacts
func (e *Engine) orderedArtifactProducers(t *Target, outRoot, logFilePath string) []lcache.ArtifactWithProducer {
	arts := t.Artifacts

	all := make([]lcache.ArtifactWithProducer, 0, len(arts.Out)+2)
	all = append(all, artifactProducer{arts.Log, logArtifact{
		LogFilePath: logFilePath,
	}})
	names := mds.Keys(arts.Out)
	names = specs.SortOutputsForHashing(names)
	for _, name := range names {
		a := arts.Out[name]
		all = append(all, artifactProducer{a.Tar(), outTarArtifact{
			Target:  t,
			Output:  name,
			OutRoot: outRoot,
		}})
		all = append(all, artifactProducer{a.Hash(), hashOutputArtifact{
			LocalState: e.LocalCache,
			Target:     t,
			Output:     name,
		}})
	}
	all = append(all, artifactProducer{arts.Manifest, manifestArtifact{
		LocalState: e.LocalCache,
		Target:     t,
	}})
	all = append(all, artifactProducer{arts.InputHash, hashInputArtifact{
		LocalState: e.LocalCache,
		Target:     t,
	}})
	return all
}
