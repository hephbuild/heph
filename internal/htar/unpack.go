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

func UnpackFromPath(ctx context.Context, path string, to hfs.FS) error {
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
	tr := tar.NewReader(r)

	var fileReader io.Reader
	err := Walk(tr, func(hdr *tar.Header, r *tar.Reader) error {
		if !match(hdr) {
			return nil
		}

		switch hdr.Typeflag {
		case tar.TypeReg:
			fileReader = r
			return ErrStopWalk
		default:
			return fmt.Errorf("is not a file, is %v: %s", hdr.Typeflag, hdr.Name)
		}
	})
	if err != nil {
		return nil, err
	}

	return fileReader, nil
}

func Unpack(ctx context.Context, r io.Reader, to hfs.FS, options ...Option) error {
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

	return Walk(tr, func(hdr *tar.Header, r *tar.Reader) error {
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
			err := to.MkdirAll(hdr.Name, hfs.FileMode(hdr.Mode)) //nolint:gosec
			if err != nil {
				return fmt.Errorf("untar: %v: %w", hdr.Name, err)
			}
		case tar.TypeSymlink:
			if hdr.Linkname == "" {
				return fmt.Errorf("untar: symlink empty for %v", hdr.Name)
			}

			if hfs.Exists(to, hdr.Name) {
				return nil
			}

			osto, ok := hfs.AsOs(to)
			if !ok {
				return fmt.Errorf("untar: unsupported symlink on fs %T", to)
			}

			err := osto.Symlink(hdr.Linkname, hdr.Name)
			if err != nil {
				return fmt.Errorf("untar: %v: %w", hdr.Name, err)
			}
		default:
			return fmt.Errorf("untar: unsupported type %v", hdr.Typeflag)
		}

		return nil
	})
}

func unpackFile(hdr *tar.Header, tr *tar.Reader, to hfs.FS, ro bool, onFile func(to string)) error {
	info, err := to.Lstat(hdr.Name)
	if err != nil && !errors.Is(err, hfs.ErrNotExist) {
		return err
	}

	if info != nil {
		// The file is probably not changed... This should prevent an infinite loop of
		// a file being codegen copy_noexclude & input to another target
		if hdr.Size == info.Size() && hdr.ModTime == info.ModTime() {
			onFile(info.Name())

			return nil
		}
	}

	err = hfs.CreateParentDir(to, hdr.Name)
	if err != nil {
		return err
	}

	f, err := hfs.Create(to, hdr.Name)
	if err != nil {
		return err
	}
	defer f.Close()

	defer onFile(f.Name())

	_, err = io.CopyN(f, tr, hdr.Size)
	if err != nil {
		return err
	}

	if osto, ok := hfs.AsOs(to); ok {
		err = osto.CloseEnsureROFD(f)
		if err != nil {
			return err
		}
	} else {
		err = f.Close()
		if err != nil {
			return err
		}
	}

	mode := os.FileMode(hdr.Mode) //nolint:gosec
	if ro {
		mode &^= 0222
	}

	if osto, ok := hfs.AsOs(to); ok {
		err = osto.Chmod(hdr.Name, mode)
		if err != nil {
			return err
		}

		err = osto.Chtimes(hdr.Name, hdr.AccessTime, hdr.ModTime)
		if err != nil {
			return err
		}
	}

	return nil
}

var ErrStopWalk = errors.New("stop walk")

func Walk(tr *tar.Reader, fs ...func(*tar.Header, *tar.Reader) error) error {
	for {
		hdr, err := tr.Next()
		if err != nil {
			if errors.Is(err, io.EOF) {
				break // End of archive
			}

			return fmt.Errorf("walk: %w", err)
		}

		for _, f := range fs {
			err = f(hdr, tr)
			if err != nil {
				if errors.Is(err, ErrStopWalk) {
					return nil
				}

				return err
			}
		}
	}

	return nil
}
