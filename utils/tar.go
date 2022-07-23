package utils

import (
	"archive/tar"
	"compress/gzip"
	"context"
	"fmt"
	log "github.com/sirupsen/logrus"
	"io"
	"io/fs"
	"os"
	"path/filepath"
)

type TarFile struct {
	From string
	To   string
}

func tarWriteEntry(file TarFile, tw *tar.Writer, info os.FileInfo) error {
	var link string
	if info.Mode().Type() == os.ModeSymlink {
		l, err := os.Readlink(file.From)
		if err != nil {
			return err
		}

		link = l

		if filepath.IsAbs(link) {
			return fmt.Errorf("absolute link not allowed")
		}
	}

	hdr, err := tar.FileInfoHeader(info, link)
	if err != nil {
		return err
	}

	hdr.Name = file.To

	if err := tw.WriteHeader(hdr); err != nil {
		return err
	}

	if !info.Mode().IsRegular() { //nothing more to do for non-regular
		return nil
	}

	f, err := os.Open(file.From)
	if err != nil {
		return err
	}
	defer f.Close()

	if _, err := io.Copy(tw, f); err != nil {
		return err
	}

	return nil
}

func tarWriteDir(file TarFile, tw *tar.Writer) error {
	return filepath.WalkDir(file.From, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}

		if d.IsDir() {
			return nil
		}

		rel, err := filepath.Rel(file.From, path)
		if err != nil {
			return err
		}

		info, err := d.Info()
		if err != nil {
			return err
		}

		return tarWriteEntry(TarFile{
			From: path,
			To:   filepath.Join(file.To, rel),
		}, tw, info)
	})
}

func Tar(ctx context.Context, files []TarFile, out string) error {
	tarf, err := os.Create(out)
	if err != nil {
		return fmt.Errorf("tar: %w", err)
	}
	defer tarf.Close()

	gw := gzip.NewWriter(tarf)
	defer gw.Close()

	return doTar(gw, files)
}

func doTar(w io.Writer, files []TarFile) error {
	tw := tar.NewWriter(w)

	for _, file := range files {
		info, err := os.Stat(file.From)
		if err != nil {
			return fmt.Errorf("tar: %w", err)
		}

		if info.IsDir() {
			err := tarWriteDir(file, tw)
			if err != nil {
				return fmt.Errorf("tar: %w", err)
			}
			continue
		}

		err = tarWriteEntry(file, tw, info)
		if err != nil {
			return fmt.Errorf("tar: %w", err)
		}
	}
	if err := tw.Close(); err != nil {
		return fmt.Errorf("tar: %w", err)
	}

	return nil
}

func Untar(ctx context.Context, in, to string) error {
	log.Tracef("untar: %v to %v", in, to)

	tarf, err := os.Open(in)
	if err != nil {
		return fmt.Errorf("untar: %w", err)
	}
	defer tarf.Close()

	gr, err := gzip.NewReader(tarf)
	if err != nil {
		return fmt.Errorf("untar: %w", err)
	}
	defer gr.Close()

	return doUntar(gr, to)
}

func doUntar(r io.Reader, to string) error {
	tr := tar.NewReader(r)

	for {
		hdr, err := tr.Next()
		if err != nil {
			if err == io.EOF {
				break // End of archive
			}

			return fmt.Errorf("untar: %w", err)
		}

		dest := filepath.Join(to, hdr.Name)

		if dir := filepath.Dir(dest); dir != "." {
			err := os.MkdirAll(dir, os.ModePerm)
			if err != nil {
				return err
			}
		}

		switch hdr.Typeflag {
		case tar.TypeReg:
			err = untarFile(hdr, tr, dest)
			if err != nil {
				return fmt.Errorf("untar: %w", err)
			}
		case tar.TypeDir:
			err := os.MkdirAll(dest, os.FileMode(hdr.Mode))
			if err != nil {
				return fmt.Errorf("untar: %w", err)
			}
		case tar.TypeSymlink:
			if hdr.Linkname == "" {
				return fmt.Errorf("untar: symlink empty for %v", hdr.Name)
			}

			err := os.Symlink(hdr.Linkname, dest)
			if err != nil {
				return fmt.Errorf("untar: %w", err)
			}
		default:
			return fmt.Errorf("untar: unsupported type %v", hdr.Typeflag)
		}
	}

	return nil
}

func untarFile(hdr *tar.Header, tr *tar.Reader, to string) error {
	f, err := os.OpenFile(to, os.O_RDWR|os.O_CREATE|os.O_TRUNC, os.FileMode(hdr.Mode))
	if err != nil {
		return err
	}
	defer f.Close()

	if _, err := io.CopyN(f, tr, hdr.Size); err != nil {
		return err
	}

	err = f.Close()
	if err != nil {
		return err
	}

	err = os.Chtimes(to, hdr.AccessTime, hdr.ModTime)
	if err != nil {
		return err
	}

	return nil
}
