package graph

import (
	"fmt"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/utils/xsync"
	"strings"
)

type ArtifactHash [2]artifacts.Artifact

func (a ArtifactHash) Hash() artifacts.Artifact {
	return a[0]
}

func (a ArtifactHash) Tar() artifacts.Artifact {
	return a[1]
}

type ArtifactRegistry struct {
	InputHash    artifacts.Artifact
	Log          artifacts.Artifact
	Manifest     artifacts.Artifact
	Out          map[string]ArtifactHash
	RestoreCache *artifacts.Artifact

	allOnce xsync.Once[[]artifacts.Artifact]
}

func (o *ArtifactRegistry) GetRestoreCache() (artifacts.Artifact, bool) {
	if o.RestoreCache == nil {
		return nil, false
	}

	return *o.RestoreCache, true
}

func (o *ArtifactRegistry) All() []artifacts.Artifact {
	return o.allOnce.MustDo(func() ([]artifacts.Artifact, error) {
		all := make([]artifacts.Artifact, 0, len(o.Out)+3)
		all = append(all, o.InputHash)
		all = append(all, o.Manifest)
		all = append(all, o.Log)
		for _, a := range o.Out {
			all = append(all, a.Tar(), a.Hash())
		}
		if a, ok := o.GetRestoreCache(); ok {
			all = append(all, a)
		}
		return all, nil
	})
}

func (o *ArtifactRegistry) OutHash(name string) artifacts.Artifact {
	return o.Out[name].Hash()
}

func (o *ArtifactRegistry) OutTar(name string) artifacts.Artifact {
	return o.Out[name].Tar()
}

func (e *State) newArtifactRegistry(target *Target) *ArtifactRegistry {
	o := &ArtifactRegistry{
		InputHash: artifacts.New("hash_input", "#input", true, false, false),
		Manifest:  artifacts.New("manifest.json", "manifest", true, false, false),
		Log:       artifacts.New("log.txt", "log", false, false, false),
		Out:       map[string]ArtifactHash{},
	}

	if target.RestoreCache.Enabled && len(target.RestoreCache.Paths) > 0 {
		name := "cache.tar"
		if k := target.RestoreCache.Key; k != "" {
			name = fmt.Sprintf("cache_%v.tar", k)
		}
		a := artifacts.New(name, "cache", false, true, true)
		o.RestoreCache = &a
	}

	for _, name := range target.OutWithSupport.Names() {
		o.Out[name] = ArtifactHash{
			artifacts.New("hash_out_"+name, strings.TrimSpace(name+" #out"), true, false, false),
			artifacts.New("out_"+name+".tar", strings.TrimSpace(name+" tar"), true, true, true),
		}
	}

	return o
}
