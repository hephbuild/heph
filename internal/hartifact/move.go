package hartifact

import (
	"fmt"
	"github.com/hephbuild/heph/internal/hproto"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func Path(a *pluginv1.Artifact) (string, error) {
	switch content := a.Content.(type) {
	case *pluginv1.Artifact_File:
		return content.File.SourcePath, nil
	case *pluginv1.Artifact_Raw:
		return "", nil
	case *pluginv1.Artifact_TargzPath:
		return content.TargzPath, nil
	case *pluginv1.Artifact_TarPath:
		return content.TarPath, nil
	default:
		return "", fmt.Errorf("unsupported content %T", a.Content)
	}
}

func Relocated(artifact *pluginv1.Artifact, to string) (*pluginv1.Artifact, error) {
	artifact = hproto.Clone(artifact)

	switch content := artifact.Content.(type) {
	case *pluginv1.Artifact_File:
		content.File.SourcePath = to
	case *pluginv1.Artifact_Raw:
		artifact.Content = &pluginv1.Artifact_File{
			File: &pluginv1.Artifact_ContentFile{
				SourcePath: to,
				OutPath:    content.Raw.Path,
			},
		}
	case *pluginv1.Artifact_TargzPath:
		content.TargzPath = to
	case *pluginv1.Artifact_TarPath:
		content.TarPath = to
	default:
		return nil, fmt.Errorf("unsupported content %T", artifact.Content)
	}

	return artifact, nil
}
