package hartifact

import (
	"archive/tar"
	"bytes"
	"context"
	"fmt"
	"io"
	"iter"
	"os"

	"github.com/hephbuild/heph/internal/hio"
	"github.com/hephbuild/heph/internal/htar"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

// Reader gives a raw io.Reader of an artifact, useful for things like hashing.
func Reader(ctx context.Context, a *pluginv1.Artifact) (io.ReadCloser, error) {
	switch a.WhichContent() {
	case pluginv1.Artifact_File_case:
		return os.Open(a.GetFile().GetSourcePath())
	case pluginv1.Artifact_Raw_case:
		return io.NopCloser(bytes.NewReader(a.GetRaw().GetData())), nil
	case pluginv1.Artifact_TargzPath_case:
		return os.Open(a.GetTargzPath())
	case pluginv1.Artifact_TarPath_case:
		return os.Open(a.GetTarPath())
	default:
		return nil, fmt.Errorf("unsupported encoding %v", a.WhichContent())
	}
}

// FileReader Assumes the output has a single file, and provides a reader for it (no matter the packaging).
func FileReader(ctx context.Context, a *pluginv1.Artifact) (io.ReadCloser, error) {
	switch a.WhichContent() {
	case pluginv1.Artifact_File_case:
		return os.Open(a.GetFile().GetSourcePath())
	case pluginv1.Artifact_Raw_case:
		return io.NopCloser(bytes.NewReader(a.GetRaw().GetData())), nil
	case pluginv1.Artifact_TarPath_case:
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
	// case *pluginv1.Artifact_TargzPath:
	default:
		return nil, fmt.Errorf("unsupported encoding %v", a.WhichContent())
	}
}

type File struct {
	io.ReadCloser
	Path string
}

// FilesReader provides a reader for each file it (no matter the packaging).
func FilesReader(ctx context.Context, a *pluginv1.Artifact) iter.Seq2[*File, error] {
	return func(yield func(*File, error) bool) {
		switch a.WhichContent() {
		case pluginv1.Artifact_File_case:
			f, err := os.Open(a.GetFile().GetSourcePath())
			if err != nil {
				if !yield(nil, err) {
					return
				}
				return
			}

			if !yield(&File{
				ReadCloser: f,
				Path:       a.GetFile().GetOutPath(),
			}, nil) {
				return
			}
		case pluginv1.Artifact_Raw_case:
			f := io.NopCloser(bytes.NewReader(a.GetRaw().GetData()))

			if !yield(&File{
				ReadCloser: f,
				Path:       a.GetRaw().GetPath(),
			}, nil) {
				return
			}
		case pluginv1.Artifact_TarPath_case:
			r, err := Reader(ctx, a)
			if err != nil {
				if !yield(nil, err) {
					return
				}
				return
			}
			defer r.Close()

			tr := tar.NewReader(r)

			err = htar.Walk(tr, func(header *tar.Header, reader *tar.Reader) error {
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
				if !yield(nil, err) {
					return
				}
				return
			}
		// case *pluginv1.Artifact_TargzPath:
		default:
			if !yield(nil, fmt.Errorf("unsupported encoding %v", a.WhichContent())) {
				return
			}
		}
	}
}
