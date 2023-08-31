package artifacts

import (
	"compress/gzip"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/xio"
	"io"
	"os"
	"path/filepath"
)

type container struct {
	name         string
	displayName  string
	genRequired  bool
	compressible bool
	manifest     bool
}

func (a container) Compressible() bool {
	return a.compressible
}

func (a container) Name() string {
	return a.name
}

func (a container) FileName() string {
	return a.name
}

func (a container) GzFileName() string {
	return a.name + ".gz"
}

func (a container) ManifestFileName() string {
	return a.name + ".manifest.json"
}

func (a container) DisplayName() string {
	return a.displayName
}

func (a container) GenRequired() bool {
	return a.genRequired
}

func (a container) GenerateManifest() bool {
	return a.manifest
}

func New(name, displayName string, genRequired, compressible, manifest bool) Artifact {
	return container{
		name:         name,
		displayName:  displayName,
		genRequired:  genRequired,
		compressible: compressible,
		manifest:     manifest,
	}
}

type Artifact interface {
	Name() string
	FileName() string
	ManifestFileName() string
	GzFileName() string
	DisplayName() string
	GenRequired() bool
	Compressible() bool
	GenerateManifest() bool
}

func ToSlice[T Artifact](as []T) []Artifact {
	return ads.Map(as, func(t T) Artifact {
		return t
	})
}

func UncompressedReaderFromArtifact(artifact Artifact, dir string) (io.ReadCloser, error) {
	uncompressedPath := filepath.Join(dir, artifact.FileName())
	f, err := os.Open(uncompressedPath)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return nil, fmt.Errorf("open: %w", err)
	}

	if f != nil {
		return f, nil
	}

	if artifact.Compressible() {
		gzPath := filepath.Join(dir, artifact.GzFileName())
		f, err := os.Open(gzPath)
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return nil, fmt.Errorf("open: %w", err)
		}

		if f != nil {
			gr, err := gzip.NewReader(f)
			if err != nil {
				_ = f.Close()
				return nil, fmt.Errorf("new: %w", err)
			}

			return xio.ReadCloser(gr, xio.MultiCloser(gr.Close, f.Close)), nil
		}
	}

	return nil, fmt.Errorf("%v: artifact not found", artifact.Name())
}
