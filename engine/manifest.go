package engine

import pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"

var ArtifactManifestName = "manifest.v1.json"

type ManifestArtifact struct {
	Hashout string

	Group    string
	Name     string
	Type     pluginv1.Artifact_Type
	Encoding pluginv1.Artifact_Encoding
}

type Manifest struct {
	Version   string
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
