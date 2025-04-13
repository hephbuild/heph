package hartifact

import (
	"context"
	"encoding/json"
	"io"
	"os"
	"time"

	"github.com/hephbuild/heph/internal/hfs"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

var ManifestName = "manifest.v1.json"

type ManifestArtifact struct {
	Hashout string

	Group    string
	Name     string
	Type     pluginv1.Artifact_Type
	Encoding pluginv1.Artifact_Encoding
}

type Manifest struct {
	Version   string
	Target    string
	CreatedAt time.Time
	Hashin    string
	Artifacts []ManifestArtifact
}

func (m Manifest) GetArtifacts(output string) []ManifestArtifact {
	a := make([]ManifestArtifact, 0)
	for _, artifact := range m.Artifacts {
		if artifact.Group != output {
			continue
		}

		a = append(a, artifact)
	}

	return a
}

func NewManifestArtifact(fs hfs.FS, m Manifest) (*pluginv1.Artifact, error) {
	b, err := json.Marshal(m) //nolint:musttag
	if err != nil {
		return nil, err
	}

	err = hfs.WriteFile(fs, ManifestName, b, os.ModePerm)
	if err != nil {
		return nil, err
	}

	return &pluginv1.Artifact{
		Name:     ManifestName,
		Type:     pluginv1.Artifact_TYPE_MANIFEST_V1,
		Encoding: pluginv1.Artifact_ENCODING_NONE,
		Uri:      "file://" + hfs.At(fs, ManifestName).Path(),
	}, nil
}

func ManifestFromArtifact(ctx context.Context, a *pluginv1.Artifact) (Manifest, error) {
	r, err := Reader(ctx, a)
	if err != nil {
		return Manifest{}, err
	}
	defer r.Close()

	return openManifest(r)
}

func ManifestFromFS(fs hfs.FS) (Manifest, error) {
	f, err := hfs.Open(fs, ManifestName)
	if err != nil {
		return Manifest{}, err
	}
	defer f.Close()

	return openManifest(f)
}

func openManifest(r io.Reader) (Manifest, error) {
	var manifest Manifest
	err := json.NewDecoder(r).Decode(&manifest) //nolint:musttag
	if err != nil {
		return Manifest{}, err
	}

	return manifest, nil
}
