package hartifact

import (
	"encoding/json"
	"fmt"
	"io"
	"time"

	"github.com/hephbuild/heph/lib/pluginsdk"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

var ManifestName = "manifest.v1.json"

type ManifestArtifactType pluginv1.Artifact_Type

func (b ManifestArtifactType) MarshalJSON() ([]byte, error) {
	v, ok := pluginv1.Artifact_Type_name[int32(b)]
	if !ok {
		return nil, fmt.Errorf("unknown artifact type %v", b)
	}

	return json.Marshal(v)
}

func (b *ManifestArtifactType) UnmarshalJSON(data []byte) error {
	var vs string
	if err := json.Unmarshal(data, &vs); err != nil {
		return err
	}

	v, ok := pluginv1.Artifact_Type_value[vs]
	if !ok {
		return fmt.Errorf("unknown artifact type %q", vs)
	}

	*b = ManifestArtifactType(v)

	return nil
}

type ManifestArtifactContentType string

const (
	ManifestArtifactContentTypeTar   ManifestArtifactContentType = "application/x-tar"
	ManifestArtifactContentTypeTarGz ManifestArtifactContentType = "application/x-gtar"
	ManifestArtifactContentTypeCpio  ManifestArtifactContentType = "application/x-cpio"
)

type ManifestArtifact struct {
	Hashout string

	Group       string
	Name        string
	Size        int64
	Type        ManifestArtifactType
	ContentType ManifestArtifactContentType
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

func EncodeManifest(w io.Writer, m *Manifest) error {
	err := json.NewEncoder(w).Encode(m) //nolint:musttag
	if err != nil {
		return err
	}

	return nil
}

func DecodeManifest(r io.Reader) (*Manifest, error) {
	var manifest Manifest
	err := json.NewDecoder(r).Decode(&manifest) //nolint:musttag
	if err != nil {
		return nil, err
	}

	return &manifest, nil
}

func ManifestContentType(a pluginsdk.Artifact) (ManifestArtifactContentType, error) {
	contentType, err := a.GetContentType()
	if err != nil {
		return "", err
	}

	switch contentType {
	case pluginsdk.ArtifactContentTypeTarGz:
		return ManifestArtifactContentTypeTarGz, nil
	case pluginsdk.ArtifactContentTypeTar:
		return ManifestArtifactContentTypeTar, nil
	case pluginsdk.ArtifactContentTypeCpio:
		return ManifestArtifactContentTypeCpio, nil
	default:
	}

	return "", fmt.Errorf("unsupported content %v", contentType)
}

func ProtoArtifactToManifest(hashout string, a pluginsdk.Artifact) (ManifestArtifact, error) {
	contentType, err := ManifestContentType(a)
	if err != nil {
		return ManifestArtifact{}, err
	}

	size, err := a.GetContentSize()
	if err != nil {
		return ManifestArtifact{}, err
	}

	return ManifestArtifact{
		Hashout:     hashout,
		Group:       a.GetGroup(),
		Name:        a.GetName(),
		Size:        size,
		Type:        ManifestArtifactType(a.GetType()),
		ContentType: contentType,
	}, nil
}
