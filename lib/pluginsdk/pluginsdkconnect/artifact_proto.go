package pluginsdkconnect

import (
	"io"
	"math"

	"github.com/hephbuild/heph/lib/pluginsdk"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

type ProtoArtifact struct {
	*pluginv1.RunRequest_Input_Artifact
}

var _ pluginsdk.Artifact = (*ProtoArtifact)(nil)

func (a ProtoArtifact) GetContentReader() (io.ReadCloser, error) {
	panic("TODO: implement call from plugin to core to get the content")
}

func (a ProtoArtifact) GetContentSize() (int64, error) {
	return math.MaxInt64, nil // will not be used
}

func (a ProtoArtifact) GetContentType() (pluginsdk.ArtifactContentType, error) {
	panic("TODO: implement call from plugin to core to get the content")
}
