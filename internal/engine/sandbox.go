package engine

import (
	"context"
	"fmt"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hfs"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func SetupSandbox(ctx context.Context, depResults []*ExecuteResultWithOrigin, fs hfs.FS) ([]*pluginv1.ArtifactWithOrigin, error) {
	var artifacts []*pluginv1.ArtifactWithOrigin

	for _, depResult := range depResults {
		for _, res := range depResult.Outputs {
			if res.Type != pluginv1.Artifact_TYPE_OUTPUT {
				return nil, fmt.Errorf("unexpected artifact type: %s", res.Type)
			}

			artifacts = append(artifacts, &pluginv1.ArtifactWithOrigin{
				Artifact: res.Artifact,
				Dep:      depResult.Origin,
			})

			listArtifact, err := SetupSandboxArtifact(ctx, res, fs)
			if err != nil {
				return nil, err
			}

			artifacts = append(artifacts, &pluginv1.ArtifactWithOrigin{
				Artifact: listArtifact,
				Dep:      depResult.Origin,
			})
		}
	}

	return artifacts, nil
}

func SetupSandboxArtifact(ctx context.Context, artifact ExecuteResultOutput, fs hfs.FS) (*pluginv1.Artifact, error) {
	listf, err := hfs.Create(fs, artifact.Hashout+".list")
	if err != nil {
		return nil, err
	}
	defer listf.Close()

	err = hartifact.Unpack(ctx, artifact.Artifact, fs, hartifact.WithOnFile(func(to string) {
		_, _ = listf.Write([]byte(to))
		_, _ = listf.Write([]byte("\n"))
	}))
	if err != nil {
		return nil, err
	}

	err = listf.Close()
	if err != nil {
		return nil, err
	}

	return &pluginv1.Artifact{
		Group:    artifact.Group,
		Name:     artifact.Name + ".list",
		Type:     pluginv1.Artifact_TYPE_OUTPUT_LIST_V1,
		Encoding: pluginv1.Artifact_ENCODING_NONE,
		Uri:      "file://" + listf.Name(),
	}, nil
}
