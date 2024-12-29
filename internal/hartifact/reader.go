package hartifact

import (
	"context"
	"fmt"
	"github.com/hephbuild/hephv2/internal/hfs"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"io"
)

func Reader(ctx context.Context, a *pluginv1.Artifact) (io.ReadCloser, error) {
	scheme, rest, err := ParseUri(a.Uri)
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

		return f, nil
	default:
		return nil, fmt.Errorf("unsupprted scheme: %s", scheme)
	}
}
