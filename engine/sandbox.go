package engine

import (
	"context"
	"fmt"
	"github.com/hephbuild/hephv2/hfs"
	"github.com/hephbuild/hephv2/htar"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"io"
)

func SetupSandbox(ctx context.Context, depResults []*ExecuteResult, fs hfs.FS) ([]*pluginv1.Artifact, error) {
	var artifacts []*pluginv1.Artifact

	for _, depResult := range depResults {
		for _, artifact := range depResult.Outputs {
			if artifact.Type != pluginv1.Artifact_TYPE_OUTPUT {
				return nil, fmt.Errorf("unexpected artifact type: %s", artifact.Type)
			}

			artifacts = append(artifacts, artifact.Artifact)

			listArtifact, err := SetupSandboxArtifact(ctx, artifact, fs)
			if err != nil {
				return nil, err
			}

			artifacts = append(artifacts, listArtifact)
		}
	}

	return artifacts, nil
}

func SetupSandboxArtifact(ctx context.Context, artifact ExecuteResultOutput, fs hfs.FS) (*pluginv1.Artifact, error) {
	scheme, rest, err := parseUri(artifact.Uri)
	if err != nil {
		return nil, err
	}

	listf, err := hfs.Create(fs, artifact.Hashout+".list")
	if err != nil {
		return nil, err
	}
	defer listf.Close()

	var r io.Reader
	switch scheme {
	case "file":
		f, err := hfs.Open(hfs.NewOS(rest), "")
		if err != nil {
			return nil, err
		}
		defer f.Close()

		r = f
	default:
		return nil, fmt.Errorf("unsupported scheme %s", scheme)
	}

	switch artifact.Encoding {
	case pluginv1.Artifact_ENCODING_NONE:
		f, err := hfs.Create(fs, artifact.Name)
		if err != nil {
			return nil, err
		}
		defer f.Close()

		_, err = io.Copy(f, r)
		if err != nil {
			return nil, err
		}
	case pluginv1.Artifact_ENCODING_TAR:
		err = htar.Unpack(ctx, r, fs, htar.WithOnFile(func(to string) {
			listf.Write([]byte(to))
			listf.Write([]byte("\n"))
		}))
		if err != nil {
			return nil, err
		}
	default:
		return nil, fmt.Errorf("unsupported encoding %s", artifact.Encoding)
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
