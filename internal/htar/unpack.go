package htar

import (
	"archive/tar"
	"context"
	"errors"
	"fmt"
	"io"
	"os"

	"github.com/hephbuild/heph/internal/hfs"
)

func UnpackFromPath(ctx context.Context, path string, to hfs.Node) error {
	f, err := os.Open(path)
	if err != nil {
		return err
	}
	defer f.Close()

	return Unpack(ctx, f, to)
}

type Option func(c *config)

func WithOnFile(onFile func(to string)) Option {
	return func(c *config) {
		c.onFile = onFile
	}
}

func WithFilter(filter func(from string) bool) Option {
	return func(c *config) {
		c.filter = filter
	}
}

type config struct {
	onFile func(to string)
	filter func(from string) bool
}

type Matcher = func(hdr *tar.Header) bool

func FileReader(ctx context.Context, r io.Reader, match Matcher) (io.Reader, error) {
	pr, pw := io.Pipe()

	go func() {
		defer pw.Close()

		tr := tar.NewReader(r)

		var matched bool
		err := Walk(tr, func(hdr *tar.Header, r io.Reader) error {
			if !match(hdr) {
				return nil
			}

			switch hdr.Typeflag {
			case tar.TypeReg:
				matched = true
				_, err := io.Copy(pw, r)
				if err != nil {
					return err
				}

				return ErrStopWalk
			default:
				return fmt.Errorf("is not a file, is %v: %s", hdr.Typeflag, hdr.Name)
			}
		})
		if err != nil {
			_ = pw.CloseWithError(err)
			return
		}

		if !matched {
			_ = pw.CloseWithError(errors.New("tar is empty"))
			return
		}
	}()

	return pr, nil
}

func Unpack(ctx context.Context, r io.Reader, to hfs.Node, options ...Option) error {
	cfg := &config{}
	for _, option := range options {
		option(cfg)
	}
	if cfg.onFile == nil {
		cfg.onFile = func(to string) {}
	}
	if cfg.filter == nil {
		cfg.filter = func(from string) bool {
			return true
		}
	}

	tr := tar.NewReader(r)

	return Walk(tr, func(hdr *tar.Header, r io.Reader) error {
		if !cfg.filter(hdr.Name) {
			return nil
		}

		switch hdr.Typeflag {
		case tar.TypeReg:
			err := unpackFile(hdr, tr, to, false, cfg.onFile)
			if err != nil {
				return fmt.Errorf("untar: %v: %w", hdr.Name, err)
			}

		case tar.TypeDir:
			err := to.At(hdr.Name).MkdirAll(hfs.FileMode(hdr.Mode)) //nolint:gosec
			if err != nil {
				return fmt.Errorf("untar: %v: %w", hdr.Name, err)
			}
		case tar.TypeSymlink:
			if hdr.Linkname == "" {
				return fmt.Errorf("untar: symlink empty for %v", hdr.Name)
			}

			if hfs.Exists(to.At(hdr.Name)) {
				return nil
			}

			osto, ok := hfs.AsOs(to)
			if !ok {
				return fmt.Errorf("untar: unsupported symlink on fs %T", to)
			}

			err := hfs.CreateParentDir(to.At(hdr.Name))
			if err != nil {
				return err
			}

			err = hfs.At(osto, hdr.Name).Symlink(hdr.Linkname)
			if err != nil {
				return fmt.Errorf("untar: %v: %w", hdr.Name, err)
			}
		default:
			return fmt.Errorf("untar: unsupported type %v", hdr.Typeflag)
		}

		return nil
	})
}

func unpackFile(hdr *tar.Header, tr io.Reader, to hfs.Node, ro bool, onFile func(to string)) error {
	fileNode := to.At(hdr.Name)

	info, err := fileNode.Lstat()
	if err != nil && !errors.Is(err, hfs.ErrNotExist) {
		return err
	}

	if info != nil {
		// The file is probably not changed... This should prevent an infinite loop of
		// a file being codegen copy_noexclude & input to another target
		if hdr.Size == info.Size() && hdr.ModTime.Equal(info.ModTime()) {
			onFile(info.Name())

			return nil
		}
	}

	err = hfs.CreateParentDir(fileNode)
	if err != nil {
		return err
	}

	f, err := hfs.Create(fileNode)
	if err != nil {
		return err
	}
	defer onFile(f.Name())
	defer f.Close()

	_, err = io.CopyN(f, tr, hdr.Size)
	if err != nil {
		return err
	}

	err = hfs.CloseEnsureROFD(f)
	if err != nil {
		return err
	}

	mode := os.FileMode(hdr.Mode) //nolint:gosec
	if ro {
		mode &^= 0222
	}

	if osFileNode, ok := hfs.AsOs(fileNode); ok {
		err = osFileNode.Chmod(mode)
		if err != nil {
			return err
		}

		err = osFileNode.Chtimes(hdr.AccessTime, hdr.ModTime)
		if err != nil {
			return err
		}
	}

	return nil
}

var ErrStopWalk = errors.New("stop walk")

func Walk(tr *tar.Reader, f func(*tar.Header, io.Reader) error) error {
	for {
		hdr, err := tr.Next()
		if err != nil {
			if errors.Is(err, io.EOF) {
				break // End of archive
			}

			return fmt.Errorf("walk: %w", err)
		}

		err = f(hdr, io.LimitReader(tr, hdr.Size))
		if err != nil {
			if errors.Is(err, ErrStopWalk) {
				break
			}

			return err
		}
	}

	return nil
}
