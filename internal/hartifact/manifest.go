package hartifact

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"time"

	"github.com/hephbuild/heph/internal/hfs"

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
)

type ManifestArtifact struct {
	Hashout string

	Group       string
	Name        string
	Type        ManifestArtifactType
	ContentType ManifestArtifactContentType
	Package     string
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

func WriteManifest(fs hfs.FS, m Manifest) (*pluginv1.Artifact, error) {
	b, err := json.Marshal(m) //nolint:musttag
	if err != nil {
		return nil, err
	}

	err = hfs.WriteFile(fs, ManifestName, b, os.ModePerm)
	if err != nil {
		return nil, err
	}

	return newManifestArtifact(fs), nil
}

func newManifestArtifact(fs hfs.FS) *pluginv1.Artifact {
	return &pluginv1.Artifact{
		Name: ManifestName,
		Type: pluginv1.Artifact_TYPE_MANIFEST_V1,
		Content: &pluginv1.Artifact_File{
			File: &pluginv1.Artifact_ContentFile{
				SourcePath: fs.Path(ManifestName),
				OutPath:    ManifestName,
			},
		},
	}
}

func ManifestFromArtifact(ctx context.Context, a *pluginv1.Artifact) (Manifest, error) {
	r, err := FileReader(ctx, a)
	if err != nil {
		return Manifest{}, err
	}
	defer r.Close()

	return DecodeManifest(r)
}

func ManifestFromFS(fs hfs.FS) (Manifest, *pluginv1.Artifact, error) {
	f, err := hfs.Open(fs, ManifestName)
	if err != nil {
		return Manifest{}, nil, err
	}
	defer f.Close()

	m, err := DecodeManifest(f)
	if err != nil {
		return Manifest{}, nil, err
	}

	return m, newManifestArtifact(fs), nil
}

func DecodeManifest(r io.Reader) (Manifest, error) {
	var manifest Manifest
	err := json.NewDecoder(r).Decode(&manifest) //nolint:musttag
	if err != nil {
		return Manifest{}, err
	}

	return manifest, nil
}

func ManifestContentType(a *pluginv1.Artifact) (ManifestArtifactContentType, error) {
	switch a.Content.(type) {
	case *pluginv1.Artifact_TargzPath:
		return ManifestArtifactContentTypeTarGz, nil
	case *pluginv1.Artifact_TarPath:
		return ManifestArtifactContentTypeTar, nil
	case *pluginv1.Artifact_File:
	case *pluginv1.Artifact_Raw:
	default:
	}

	return "", fmt.Errorf("unsupported content %T", a.Content)
}

func ProtoArtifactToManifest(hashout string, artifact *pluginv1.Artifact) (ManifestArtifact, error) {
	contentType, err := ManifestContentType(artifact)
	if err != nil {
		return ManifestArtifact{}, err
	}

	return ManifestArtifact{
		Hashout:     hashout,
		Group:       artifact.Group,
		Name:        artifact.Name,
		Type:        ManifestArtifactType(artifact.Type),
		ContentType: contentType,
	}, nil
}

func ManifestArtifactToProto(artifact ManifestArtifact, path string) (*pluginv1.Artifact, error) {
	partifact := &pluginv1.Artifact{
		Group: artifact.Group,
		Name:  artifact.Name,
		Type:  pluginv1.Artifact_Type(artifact.Type),
	}

	switch artifact.ContentType {
	case ManifestArtifactContentTypeTar:
		partifact.Content = &pluginv1.Artifact_TarPath{
			TarPath: path,
		}
	case ManifestArtifactContentTypeTarGz:
		partifact.Content = &pluginv1.Artifact_TargzPath{
			TargzPath: path,
		}
	default:
		return nil, fmt.Errorf("unsupported content type %q", artifact.ContentType)
	}

	return partifact, nil
}
