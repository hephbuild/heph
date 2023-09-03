package targetrun

import (
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/lcache"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/mds"
	"github.com/hephbuild/heph/utils/xfs"
)

type artifactProducer struct {
	artifacts.Artifact
	lcache.ArtifactProducer
}

func (e *Runner) artifactWithProducers(t *lcache.Target, outRoot, logFilePath string) []lcache.ArtifactWithProducer {
	arts := t.Artifacts

	all := make([]lcache.ArtifactWithProducer, 0, len(arts.Out)+4)
	all = append(all, artifactProducer{arts.Log, logArtifact{
		LogFilePath: logFilePath,
	}})

	outputs := mds.Keys(arts.Out)
	outputs = specs.SortOutputsForHashing(outputs)

	for _, output := range outputs {
		a := arts.Out[output]

		var rpaths xfs.RelPaths
		if output == specs.SupportFilesOutput {
			rpaths = t.ActualSupportFiles()
		} else {
			rpaths = t.ActualOutFiles().Name(output)
		}
		all = append(all, artifactProducer{a.Tar(), outTarArtifact{
			Target:  t,
			Name:    a.Tar().Name(),
			Paths:   rpaths,
			OutRoot: outRoot,
		}})
		all = append(all, artifactProducer{a.Hash(), hashOutputArtifact{
			LocalState: e.LocalCache,
			Target:     t,
			Output:     output,
		}})
	}
	if a, ok := arts.GetRestoreCache(); ok {
		all = append(all, artifactProducer{a, outTarArtifact{
			Target:    t,
			Name:      a.Name(),
			Paths:     t.ActualRestoreCacheFiles(),
			OutRoot:   outRoot,
			SkipEmpty: true,
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
