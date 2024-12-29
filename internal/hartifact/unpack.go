package hartifact

import (
	"context"
	"fmt"
	"github.com/hephbuild/hephv2/internal/hfs"
	"github.com/hephbuild/hephv2/internal/htar"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"io"
	"strings"
)

func ParseUri(uri string) (string, string, error) {
	scheme, rest, ok := strings.Cut(uri, "://")
	if !ok {
		return "", "", fmt.Errorf("invalid URI: %s", uri)
	}

	return scheme, rest, nil
}

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

	switch artifact.Encoding {
	case pluginv1.Artifact_ENCODING_NONE:
		f, err := hfs.Create(fs, artifact.Name)
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
	default:
		return fmt.Errorf("unsupported encoding %s", artifact.Encoding)
	}

	return nil
}
