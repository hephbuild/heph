package engine

import (
	"github.com/hephbuild/heph/engine/artifacts"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/utils"
	"strings"
)

type ArtifactsOut [2]artifacts.Artifact

func (a ArtifactsOut) Hash() artifacts.Artifact {
	return a[0]
}

func (a ArtifactsOut) Tar() artifacts.Artifact {
	return a[1]
}

type ArtifactOrchestrator struct {
	InputHash artifacts.Artifact
	Log       artifacts.Artifact
	Manifest  artifacts.Artifact
	Out       map[string]ArtifactsOut

	allOnce        utils.Once[[]artifacts.Artifact]
	allReverseOnce utils.Once[[]artifacts.Artifact]
}

func (o *ArtifactOrchestrator) All() []artifacts.Artifact {
	return o.allOnce.MustDo(func() ([]artifacts.Artifact, error) {
		all := make([]artifacts.Artifact, 0, len(o.Out)+2)
		all = append(all, o.InputHash)
		all = append(all, o.Manifest)
		all = append(all, o.Log)
		for _, a := range o.Out {
			all = append(all, a.Tar(), a.Hash())
		}
		return all, nil
	})
}

// AllStore returns artifact in hashing order
// For hashing to work properly, support_files tar must go first, then support_files hash
// then the other artifacts
func (o *ArtifactOrchestrator) AllStore() []artifacts.Artifact {
	return o.allReverseOnce.MustDo(func() ([]artifacts.Artifact, error) {
		all := make([]artifacts.Artifact, 0, len(o.Out)+2)
		all = append(all, o.Log)
		names := utils.Keys(o.Out)
		names = targetspec.SortOutputsForHashing(names)
		for _, name := range names {
			a := o.Out[name]
			all = append(all, a.Tar(), a.Hash())
		}
		all = append(all, o.Manifest)
		all = append(all, o.InputHash)
		return all, nil
	})
}

func (o *ArtifactOrchestrator) OutHash(name string) artifacts.Artifact {
	return o.Out[name].Hash()
}

func (o *ArtifactOrchestrator) OutTar(name string) artifacts.Artifact {
	return o.Out[name].Tar()
}

func (e *Engine) newArtifactOrchestrator(target *Target) *ArtifactOrchestrator {
	o := &ArtifactOrchestrator{
		InputHash: artifacts.New("hash_input", "#input", true, hashInputArtifact{
			Engine: e,
			Target: target,
		}),
		Manifest: artifacts.New("manifest.json", "manifest", true, manifestArtifact{
			Engine: e,
			Target: target,
		}),
		Log: artifacts.New("log.tar.gz", "log", false, logArtifact{}),
		Out: map[string]ArtifactsOut{},
	}

	names := target.OutWithSupport.Names()
	names = targetspec.SortOutputsForHashing(names)

	for _, name := range names {
		o.Out[name] = ArtifactsOut{
			artifacts.New("hash_out_"+name, strings.TrimSpace(name+" #out"), true, hashOutputArtifact{
				Engine: e,
				Target: target,
				Output: name,
			}),
			artifacts.New("out_"+name+".tar.gz", strings.TrimSpace(name+" tar.gz"), true, outTarArtifact{
				Target: target,
				Output: name,
			}),
		}
	}

	return o
}
