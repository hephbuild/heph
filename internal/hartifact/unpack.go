package hartifact

import (
	"context"
	"fmt"
	"io"

	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/htar"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

type unpackConfig struct {
	onFile func(to string)
	filter func(from string) bool
}

type UnpackOption func(*unpackConfig)

func WithOnFile(onFile func(to string)) UnpackOption {
	return func(config *unpackConfig) {
		config.onFile = onFile
	}
}

func WithFilter(filter func(from string) bool) UnpackOption {
	return func(config *unpackConfig) {
		config.filter = filter
	}
}

func Unpack(ctx context.Context, artifact *pluginv1.Artifact, fs hfs.FS, options ...UnpackOption) error {
	var cfg unpackConfig
	for _, option := range options {
		option(&cfg)
	}
	if cfg.onFile == nil {
		cfg.onFile = func(to string) {}
	}
	if cfg.filter == nil {
		cfg.filter = func(from string) bool {
			return true
		}
	}

	r, err := Reader(ctx, artifact)
	if err != nil {
		return err
	}
	defer r.Close()

	fs = hfs.At(fs, artifact.Package)

	switch artifact.GetEncoding() {
	case pluginv1.Artifact_ENCODING_NONE:
		if !cfg.filter(artifact.GetName()) {
			return nil
		}

		f, err := hfs.Create(fs, artifact.GetName())
		if err != nil {
			return err
		}
		defer f.Close()
		defer cfg.onFile(f.Name())

		_, err = io.Copy(f, r)
		if err != nil {
			return err
		}
	case pluginv1.Artifact_ENCODING_TAR:
		err = htar.Unpack(ctx, r, fs, htar.WithOnFile(cfg.onFile), htar.WithFilter(cfg.filter))
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
