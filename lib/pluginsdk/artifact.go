package pluginsdk

import (
	"io"

	"github.com/hephbuild/heph/internal/hfs"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

type ArtifactContentType string

const (
	ArtifactContentTypeTar   ArtifactContentType = "application/x-tar"
	ArtifactContentTypeTarGz ArtifactContentType = "application/x-gtar"
)

type Artifact interface {
	GetGroup() string
	GetName() string
	GetType() pluginv1.Artifact_Type
	GetContentReader() (io.ReadCloser, error)
	GetContentSize() (int64, error)
	GetContentType() (ArtifactContentType, error)
}

type FSArtifact interface {
	Artifact
	FSNode() hfs.Node
}

type ArtifactWithOrigin struct {
	Artifact
	Origin *pluginv1.TargetDef_InputOrigin
}

func (a ArtifactWithOrigin) GetArtifact() Artifact {
	return a.Artifact
}

func (a ArtifactWithOrigin) GetOrigin() *pluginv1.TargetDef_InputOrigin {
	return a.Origin
}
