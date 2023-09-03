package targetrun

import (
	"errors"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/lcache"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/mds"
	"github.com/hephbuild/heph/utils/xfs"
	"os"
)

type artifactProducer struct {
	artifacts.Artifact
	lcache.ArtifactProducer
}

// orderedArtifactProducers returns artifact in hashing order
// For hashing to work properly, support_files tar must go first, then support_files hash
// then the other artifacts
func (e *Runner) orderedArtifactProducers(t *lcache.Target, outRoot, logFilePath string) []lcache.ArtifactWithProducer {
	arts := t.Artifacts

	all := make([]lcache.ArtifactWithProducer, 0, len(arts.Out)+2)
	all = append(all, artifactProducer{arts.Log, logArtifact{
		LogFilePath: logFilePath,
	}})
	names := mds.Keys(arts.Out)
	names = specs.SortOutputsForHashing(names)
	for _, name := range names {
		a := arts.Out[name]

		var rpaths xfs.RelPaths
		if name == specs.SupportFilesOutput {
			rpaths = t.ActualSupportFiles()
		} else {
			rpaths = t.ActualOutFiles().Name(name)
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
			Output:     name,
		}})
	}
	if a, ok := arts.GetRestoreCache(); ok {
		all = append(all, artifactProducer{a, outTarArtifact{
			Target:    t,
			Name:      a.Name(),
			Paths:     t.ActualRestoreCacheFiles(),
			OutRoot:   outRoot,
			SkipEmpty: true,
			OnStatErr: func(err error) (bool, error) {
				if errors.Is(err, os.ErrNotExist) {
					return true, nil
				}

				return false, err
			},
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
