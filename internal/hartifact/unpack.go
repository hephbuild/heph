package hartifact

import (
	"context"
	"github.com/hephbuild/hephv2/internal/hfs"
	"github.com/hephbuild/hephv2/internal/htar"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"strings"
)

func Unpack(ctx context.Context, a *pluginv1.Artifact, fs hfs.FS) error {
	path := strings.TrimPrefix(a.Uri, "file://")

	err := htar.UnpackFromPath(ctx, path, fs)
	if err != nil {
		return err
	}

	return nil
}
