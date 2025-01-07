package hartifact

import (
	"context"
	"fmt"
	"io"

	"github.com/hephbuild/heph/internal/hio"
	"github.com/hephbuild/heph/internal/htar"

	"github.com/hephbuild/heph/internal/hfs"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func subpathReader(ctx context.Context, artifact *pluginv1.Artifact, r io.ReadCloser, path string) (io.ReadCloser, error) {
	if path == "" {
		return r, nil
	}

	switch artifact.GetEncoding() {
	case pluginv1.Artifact_ENCODING_NONE:
		return r, nil
	case pluginv1.Artifact_ENCODING_TAR:
		f, err := htar.FileReader(ctx, r, path)
		if err != nil {
			return nil, err
		}

		return hio.NewReadCloser(f, r), nil
	case pluginv1.Artifact_ENCODING_BASE64, pluginv1.Artifact_ENCODING_TAR_GZ, pluginv1.Artifact_ENCODING_UNSPECIFIED:
		fallthrough
	default:
		return nil, fmt.Errorf("unsupported encoding %s", artifact.GetEncoding())
	}
}

func SubpathReader(ctx context.Context, a *pluginv1.Artifact, path string) (io.ReadCloser, error) {
	scheme, rest, err := ParseURI(a.GetUri())
	if err != nil {
		return nil, err
	}

	switch scheme {
	case "file":
		fromfs := hfs.NewOS(rest)

		f, err := hfs.Open(fromfs, "")
		if err != nil {
			return nil, err
		}

		return subpathReader(ctx, a, f, path)
	default:
		return nil, fmt.Errorf("unsupported scheme: %s", scheme)
	}
}

func Reader(ctx context.Context, a *pluginv1.Artifact) (io.ReadCloser, error) {
	return SubpathReader(ctx, a, "")
}

func ReadAll(ctx context.Context, a *pluginv1.Artifact, path string) ([]byte, error) {
	r, err := SubpathReader(ctx, a, path)
	if err != nil {
		return nil, err
	}
	defer r.Close()

	return io.ReadAll(r)
}
