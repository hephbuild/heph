package hartifact

import (
	"archive/tar"
	"bytes"
	"context"
	"fmt"
	"io"
	"os"

	"github.com/hephbuild/heph/internal/hio"
	"github.com/hephbuild/heph/internal/htar"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

// Reader gives a raw io.Reader of an artifact, useful for things like hashing
func Reader(ctx context.Context, a *pluginv1.Artifact) (io.ReadCloser, error) {
	switch content := a.Content.(type) {
	case *pluginv1.Artifact_File:
		return os.Open(content.File.SourcePath)
	case *pluginv1.Artifact_Raw:
		return io.NopCloser(bytes.NewReader(content.Raw.Data)), nil
	case *pluginv1.Artifact_TargzPath:
		return os.Open(content.TargzPath)
	case *pluginv1.Artifact_TarPath:
		return os.Open(content.TarPath)
	default:
		return nil, fmt.Errorf("unsupported encoding %T", a.Content)
	}
}

// FileReader Assumes the output has a single file, and provides a reader for it (no matter the packaging)
func FileReader(ctx context.Context, a *pluginv1.Artifact) (io.ReadCloser, error) {
	switch content := a.Content.(type) {
	case *pluginv1.Artifact_File:
		return os.Open(content.File.SourcePath)
	case *pluginv1.Artifact_Raw:
		return io.NopCloser(bytes.NewReader(content.Raw.Data)), nil
	case *pluginv1.Artifact_TarPath:
		r, err := Reader(ctx, a)
		if err != nil {
			return nil, err
		}

		tr, err := htar.FileReader(ctx, r, func(hdr *tar.Header) bool {
			return true
		})
		if err != nil {
			return nil, err
		}

		return hio.NewReadCloser(tr, r), nil
	//case *pluginv1.Artifact_TargzPath:
	default:
		return nil, fmt.Errorf("unsupported encoding %T", a.Content)
	}
}
