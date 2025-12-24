package hartifact

import (
	"context"
	"fmt"
	"io"

	"github.com/hephbuild/heph/internal/htar"

	"github.com/hephbuild/heph/internal/hfs"
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

	switch artifact.WhichContent() {
	case pluginv1.Artifact_File_case:
		if !cfg.filter(artifact.GetName()) {
			return nil
		}

		create := hfs.Create
		if artifact.GetFile().GetX() {
			create = hfs.CreateExec
		}

		f, err := create(fs, artifact.GetFile().GetOutPath())
		if err != nil {
			return fmt.Errorf("file: create: %w", err)
		}
		defer cfg.onFile(f.Name())
		defer f.Close()

		_, err = io.Copy(f, r)
		if err != nil {
			return err
		}

		err = hfs.CloseEnsureROFD(f)
		if err != nil {
			return err
		}
	case pluginv1.Artifact_Raw_case:
		create := hfs.Create
		if artifact.GetRaw().GetX() {
			create = hfs.CreateExec
		}

		f, err := create(fs, artifact.GetRaw().GetPath())
		if err != nil {
			return fmt.Errorf("raw: create: %w", err)
		}
		defer cfg.onFile(f.Name())
		defer f.Close()

		_, err = io.Copy(f, r)
		if err != nil {
			return err
		}

		err = hfs.CloseEnsureROFD(f)
		if err != nil {
			return err
		}
	case pluginv1.Artifact_TarPath_case:
		err = htar.Unpack(ctx, r, fs, htar.WithOnFile(cfg.onFile), htar.WithFilter(cfg.filter))
		if err != nil {
			return fmt.Errorf("tar: %w", err)
		}
	// case *pluginv1.Artifact_TargzPath:
	default:
		return fmt.Errorf("unsupported encoding %v", artifact.WhichContent())
	}

	return nil
}
