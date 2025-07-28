package hartifact

import (
	"fmt"

	"github.com/hephbuild/heph/internal/htypes"

	"github.com/hephbuild/heph/internal/hproto"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func Path(a *pluginv1.Artifact) (string, error) {
	switch a.WhichContent() {
	case pluginv1.Artifact_File_case:
		return a.GetFile().GetSourcePath(), nil
	case pluginv1.Artifact_Raw_case:
		return "", nil
	case pluginv1.Artifact_TargzPath_case:
		return a.GetTargzPath(), nil
	case pluginv1.Artifact_TarPath_case:
		return a.GetTarPath(), nil
	default:
		return "", fmt.Errorf("unsupported content %v", a.WhichContent())
	}
}

func Relocated(artifact *pluginv1.Artifact, to string) (*pluginv1.Artifact, error) {
	artifact = hproto.Clone(artifact)

	switch artifact.WhichContent() {
	case pluginv1.Artifact_File_case:
		if x := htypes.Ptr(to); x != nil {
			artifact.GetFile().SetSourcePath(*x)
		} else {
			artifact.GetFile().ClearSourcePath()
		}
	case pluginv1.Artifact_Raw_case:
		return nil, fmt.Errorf("unsupported content %v", artifact.WhichContent())
	case pluginv1.Artifact_TargzPath_case:
		artifact.SetTargzPath(to)
	case pluginv1.Artifact_TarPath_case:
		artifact.SetTarPath(to)
	default:
		return nil, fmt.Errorf("unsupported content %v", artifact.WhichContent())
	}

	return artifact, nil
}
