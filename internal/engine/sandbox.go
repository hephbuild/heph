package engine

import (
	"context"
	"fmt"
	"os"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hfs"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func SetupSandbox(ctx context.Context, def *LightLinkedTarget, depResults []*ExecuteResultWithOrigin, workdirfs, cwdfs hfs.FS) ([]*pluginv1.ArtifactWithOrigin, error) {
	err := workdirfs.MkdirAll(def.Ref.GetPackage(), hfs.ModePerm)
	if err != nil {
		return nil, err
	}

	err = cwdfs.MkdirAll("", os.ModePerm)
	if err != nil {
		return nil, err
	}

	var artifacts []*pluginv1.ArtifactWithOrigin

	for _, depResult := range depResults {
		for _, res := range depResult.Artifacts {
			if res.Type != pluginv1.Artifact_TYPE_OUTPUT {
				return nil, fmt.Errorf("unexpected artifact type: %s", res.Type)
			}

			artifacts = append(artifacts, &pluginv1.ArtifactWithOrigin{
				Artifact: res.Artifact,
				Dep:      depResult.Origin,
			})

			listArtifact, err := SetupSandboxArtifact(ctx, res, workdirfs)
			if err != nil {
				return nil, err
			}

			artifacts = append(artifacts, &pluginv1.ArtifactWithOrigin{
				Artifact: listArtifact,
				Dep:      depResult.Origin,
			})
		}
	}

	for _, output := range def.CollectOutputs {
		for _, path := range output.GetPaths() {
			err := hfs.CreateParentDir(cwdfs, path)
			if err != nil {
				return nil, err
			}
		}
	}

	return artifacts, nil
}

func SetupSandboxArtifact(ctx context.Context, artifact ExecuteResultArtifact, fs hfs.FS) (*pluginv1.Artifact, error) {
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
