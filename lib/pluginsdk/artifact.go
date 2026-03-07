package pluginsdk

import (
	"io"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

type Artifact interface {
	GetGroup() string
	GetName() string
	GetType() pluginv1.Artifact_Type
	GetContentReader() (io.ReadCloser, error)
	GetContentSize() (int64, error)

	GetProto() *pluginv1.Artifact
}

var _ Artifact = (*ProtoArtifact)(nil)

type ProtoArtifact struct {
	*pluginv1.Artifact
	ContentReaderFunc func(e ProtoArtifact) (io.ReadCloser, error)
	ContentSizeFunc   func(e ProtoArtifact) (int64, error)
}

func (e ProtoArtifact) GetProto() *pluginv1.Artifact {
	return e.Artifact
}

func (e ProtoArtifact) GetContentReader() (io.ReadCloser, error) {
	return e.ContentReaderFunc(e)
}
func (e ProtoArtifact) GetContentSize() (int64, error) {
	return e.ContentSizeFunc(e)
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
