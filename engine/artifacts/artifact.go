package artifacts

import (
	"compress/gzip"
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/flock"
	"github.com/hephbuild/heph/utils/fs"
	"github.com/hephbuild/heph/utils/hio"
	"go.uber.org/multierr"
	"io"
	"os"
	"path/filepath"
)

var Skip = errors.New("skip artifact")

type container struct {
	Producer
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

func New(name, displayName string, genRequired, compressible bool, artifact Producer) Artifact {
	return container{
		Producer:     artifact,
		name:         name,
		displayName:  displayName,
		genRequired:  genRequired,
		compressible: compressible,
	}
}

type GenContext struct {
	OutRoot     string
	LogFilePath string

	w              io.WriteCloser
	accessedWriter bool
}

func (g *GenContext) Writer() io.Writer {
	g.accessedWriter = true
	return g.w
}

type Producer interface {
	Gen(ctx context.Context, gctx *GenContext) error
}

type Artifact interface {
	Producer
	Name() string
	FileName() string
	GzFileName() string
	DisplayName() string
	GenRequired() bool
	Compressible() bool
}

type Func struct {
	Func func(ctx context.Context, gctx *GenContext) error
}

func (a Func) Gen(ctx context.Context, gctx *GenContext) error {
	return a.Func(ctx, gctx)
}

func GenArtifact(ctx context.Context, dir string, a Artifact, gctx GenContext, compress bool) (string, error) {
	shouldCompress := a.Compressible() && compress

	p := filepath.Join(dir, a.FileName())
	if shouldCompress {
		p = filepath.Join(dir, a.GzFileName())
	}

	tmpp := fs.ProcessUniquePath(p)
	defer os.Remove(tmpp)

	f, err := os.Create(tmpp)
	if err != nil {
		return "", err
	}
	defer f.Close()

	go func() {
		<-ctx.Done()
		_ = f.Close()
	}()

	gctx.w = f

	if shouldCompress {
		gw := gzip.NewWriter(f)
		defer gw.Close()

		gctx.w = gw
	}

	err = a.Gen(ctx, &gctx)
	if err != nil {
		if errors.Is(err, Skip) {
			if a.GenRequired() {
				return "", fmt.Errorf("%v is required, but returned: %w", a.Name(), err)
			}
			return "", nil
		}
		return "", fmt.Errorf("%v: %w", a.Name(), err)
	}

	if !gctx.accessedWriter {
		return "", fmt.Errorf("%v did not produce output", a.Name())
	}

	_ = gctx.w.Close()
	_ = f.Close()

	err = os.Rename(tmpp, p)
	if err != nil {
		return "", err
	}

	return p, nil
}

func UncompressedPathFromArtifact(ctx context.Context, artifact Artifact, dir string) (string, error) {
	uncompressedPath := filepath.Join(dir, artifact.FileName())
	if fs.PathExists(uncompressedPath) {
		return uncompressedPath, nil
	}

	if artifact.Compressible() {
		gzPath := filepath.Join(dir, artifact.GzFileName())
		if fs.PathExists(gzPath) {
			l := flock.NewFlock("", gzPath+".lock")
			err := l.Lock(ctx)
			if err != nil {
				return "", err
			}

			defer func() {
				err := l.Unlock()
				if err != nil {
					log.Errorf("unlock: %v: %v", gzPath, err)
				}
			}()

			if fs.PathExists(uncompressedPath) {
				return uncompressedPath, nil
			}

			log.Debugf("ungz %v to %v", gzPath, uncompressedPath)

			tmpp := fs.ProcessUniquePath(uncompressedPath)

			gf, err := os.Open(gzPath)
			if err != nil {
				return "", err
			}
			defer gf.Close()

			gfc, cancel := hio.ContextReader(ctx, gf)
			defer cancel()

			tf, err := os.Create(tmpp)
			if err != nil {
				return "", err
			}
			defer tf.Close()

			gr, err := gzip.NewReader(gfc)
			if err != nil {
				return "", fmt.Errorf("ungz: reader: %w", err)
			}
			defer gr.Close()

			_, err = io.Copy(tf, gr)
			if err != nil {
				return "", fmt.Errorf("ungz: cp: %w", err)
			}

			_ = gr.Close()
			_ = tf.Close()
			_ = gf.Close()

			err = os.Rename(tmpp, uncompressedPath)
			if err != nil {
				return "", err
			}

			return uncompressedPath, nil
		}
	}

	return "", fmt.Errorf("%v: artifact not found", artifact.Name())
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
	if fs.PathExists(uncompressedPath) {
		f, err := os.Open(uncompressedPath)
		if err != nil {
			return nil, err
		}

		return f, nil
	}

	if artifact.Compressible() {
		gzPath := filepath.Join(dir, artifact.GzFileName())
		if fs.PathExists(gzPath) {
			f, err := os.Open(gzPath)
			if err != nil {
				return nil, err
			}

			gr, err := gzip.NewReader(f)
			if err != nil {
				return nil, err
			}

			return readerMultiCloser{
				Reader: gr,
				cs:     []io.Closer{gr, f},
			}, nil
		}
	}

	return nil, fmt.Errorf("%v: artifact not found", artifact.Name())
}
