package hartifact

import (
	"archive/tar"
	"context"
	"fmt"
	"io"
	"iter"

	"github.com/hephbuild/heph/internal/hcpio"
	"github.com/hephbuild/heph/internal/hio"
	"github.com/hephbuild/heph/internal/htar"
	"github.com/hephbuild/heph/lib/pluginsdk"
	"github.com/unikraft/go-cpio"
)

// FileReader Assumes the output has a single file, and provides a reader for it (no matter the packaging).
func FileReader(ctx context.Context, a pluginsdk.Artifact) (io.ReadCloser, error) {
	contentType, err := a.GetContentType()
	if err != nil {
		return nil, fmt.Errorf("get content type: %w", err)
	}

	switch contentType {
	case pluginsdk.ArtifactContentTypeTar:
		r, err := a.GetContentReader()
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
	case pluginsdk.ArtifactContentTypeCpio:
		r, err := a.GetContentReader()
		if err != nil {
			return nil, err
		}

		cr, err := hcpio.FileReader(ctx, r, func(hdr *hcpio.Header) bool {
			return true
		})
		if err != nil {
			return nil, err
		}

		return hio.NewReadCloser(cr, r), nil
	default:
		return nil, fmt.Errorf("unsupported encoding %v", contentType)
	}
}

// FileReader Assumes the output has a single file, and provides the bytes for it (no matter the packaging).
func FileReadAll(ctx context.Context, a pluginsdk.Artifact) ([]byte, error) {
	f, err := FileReader(ctx, a)
	if err != nil {
		return nil, err
	}
	defer f.Close()

	return io.ReadAll(f)
}

type File struct {
	io.ReadCloser
	Path string
}

// FilesReader provides a reader for each file it (no matter the packaging).
func FilesReader(ctx context.Context, a pluginsdk.Artifact) iter.Seq2[*File, error] {
	return func(yield func(*File, error) bool) {
		contentType, err := a.GetContentType()
		if err != nil {
			yield(nil, fmt.Errorf("get content type: %w", err))
			return
		}

		switch contentType {
		case pluginsdk.ArtifactContentTypeTar:
			r, err := a.GetContentReader()
			if err != nil {
				yield(nil, err)
				return
			}
			defer r.Close()

			tr := tar.NewReader(r)

			err = htar.Walk(tr, func(header *tar.Header, reader io.Reader) error {
				if header.Typeflag != tar.TypeReg {
					return nil
				}

				if !yield(&File{
					ReadCloser: io.NopCloser(reader),
					Path:       header.Name,
				}, nil) {
					return htar.ErrStopWalk
				}

				return nil
			})
			if err != nil {
				yield(nil, err)
				return
			}

			return
		case pluginsdk.ArtifactContentTypeCpio:
			r, err := a.GetContentReader()
			if err != nil {
				yield(nil, err)
				return
			}
			defer r.Close()

			cr := cpio.NewReader(r)

			err = hcpio.Walk(cr, func(header *cpio.Header, reader io.Reader) error {
				if header.Mode&cpio.TypeRegular == 0 {
					return nil
				}

				if !yield(&File{
					ReadCloser: io.NopCloser(reader),
					Path:       header.Name,
				}, nil) {
					return hcpio.ErrStopWalk
				}

				return nil
			})
			if err != nil {
				yield(nil, err)
				return
			}

			return
		default:
			yield(nil, fmt.Errorf("unsupported encoding %v", contentType))
			return
		}
	}
}
