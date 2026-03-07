package hartifact

import (
	"context"
	"fmt"

	"github.com/hephbuild/heph/internal/htar"
	"github.com/hephbuild/heph/lib/pluginsdk"

	"github.com/hephbuild/heph/internal/hfs"
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

func Unpack(ctx context.Context, artifact pluginsdk.Artifact, node hfs.Node, options ...UnpackOption) error {
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

	r, err := artifact.GetContentReader()
	if err != nil {
		return err
	}
	defer r.Close()

	contentType, err := artifact.GetContentType()
	if err != nil {
		return err
	}

	switch contentType {
	case pluginsdk.ArtifactContentTypeTar:
		err = htar.Unpack(ctx, r, node, htar.WithOnFile(cfg.onFile), htar.WithFilter(cfg.filter))
		if err != nil {
			return fmt.Errorf("tar: %w", err)
		}

		return nil
	// case *pluginv1.Artifact_TargzPath:
	default:
		return fmt.Errorf("unsupported encoding %v", contentType)
	}
}
