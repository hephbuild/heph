package artifacts

import (
	"compress/gzip"
	"errors"
	"fmt"
	"go.uber.org/multierr"
	"io"
	"os"
	"path/filepath"
)

type container struct {
	name         string
	displayName  string
	genRequired  bool
	compressible bool
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

func (a container) DisplayName() string {
	return a.displayName
}

func (a container) GenRequired() bool {
	return a.genRequired
}

func New(name, displayName string, genRequired, compressible bool) Artifact {
	return container{
		name:         name,
		displayName:  displayName,
		genRequired:  genRequired,
		compressible: compressible,
	}
}

type Artifact interface {
	Name() string
	FileName() string
	GzFileName() string
	DisplayName() string
	GenRequired() bool
	Compressible() bool
}

type readerMultiCloser struct {
	io.Reader
	cs []io.Closer
}

func (r readerMultiCloser) Close() error {
	var err error

	for _, c := range r.cs {
		cerr := c.Close()
		if cerr != nil {
			err = multierr.Append(err, cerr)
		}
	}

	return err
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

			return readerMultiCloser{
				Reader: gr,
				cs:     []io.Closer{gr, f},
			}, nil
		}
	}

	return nil, fmt.Errorf("%v: artifact not found", artifact.Name())
}
