package graph

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

func (o *ArtifactOrchestrator) OutHash(name string) artifacts.Artifact {
	return o.Out[name].Hash()
}

func (o *ArtifactOrchestrator) OutTar(name string) artifacts.Artifact {
	return o.Out[name].Tar()
}

func (e *State) newArtifactOrchestrator(target *Target) *ArtifactOrchestrator {
	o := &ArtifactOrchestrator{
		InputHash: artifacts.New("hash_input", "#input", true, false),
		Manifest:  artifacts.New("manifest.json", "manifest", true, false),
		Log:       artifacts.New("log.txt", "log", false, false),
		Out:       map[string]ArtifactsOut{},
	}

	names := target.OutWithSupport.Names()
	names = targetspec.SortOutputsForHashing(names)

	for _, name := range names {
		o.Out[name] = ArtifactsOut{
			artifacts.New("hash_out_"+name, strings.TrimSpace(name+" #out"), true, false),
			artifacts.New("out_"+name+".tar", strings.TrimSpace(name+" tar"), true, true),
		}
	}

	return o
}