package hartifact

import (
	"context"
	"fmt"
	"io"

	"github.com/hephbuild/hephv2/internal/hfs"
	"github.com/hephbuild/hephv2/internal/htar"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
)

type unpackConfig struct {
	onFile func(to string)
}

type UnpackOption func(*unpackConfig)

func WithOnFile(onFile func(to string)) UnpackOption {
	return func(config *unpackConfig) {
		config.onFile = onFile
	}
}

func Unpack(ctx context.Context, artifact *pluginv1.Artifact, fs hfs.FS, options ...UnpackOption) error {
	var cfg unpackConfig
	for _, option := range options {
		option(&cfg)
	}

	r, err := Reader(ctx, artifact)
	if err != nil {
		return err
	}
	defer r.Close()

	switch artifact.GetEncoding() {
	case pluginv1.Artifact_ENCODING_NONE:
		f, err := hfs.Create(fs, artifact.GetName())
		if err != nil {
			return err
		}
		defer f.Close()

		_, err = io.Copy(f, r)
		if err != nil {
			return err
		}
	case pluginv1.Artifact_ENCODING_TAR:
		err = htar.Unpack(ctx, r, fs, htar.WithOnFile(cfg.onFile))
		if err != nil {
			return err
		}
	case pluginv1.Artifact_ENCODING_BASE64, pluginv1.Artifact_ENCODING_TAR_GZ, pluginv1.Artifact_ENCODING_UNSPECIFIED:
		fallthrough
	default:
		return fmt.Errorf("unsupported encoding %s", artifact.GetEncoding())
	}

	return nil
}
