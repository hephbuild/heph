package htar

import (
	"archive/tar"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"

	"github.com/hephbuild/heph/internal/hfs"
)

type Packer struct {
	tw           *tar.Writer
	AllowAbsLink bool
}

func NewPacker(w io.Writer) *Packer {
	return &Packer{tw: tar.NewWriter(w)}
}

func (p *Packer) WriteFile(f hfs.File, path string) error {
	info, err := f.LStat()
	if err != nil {
		return err
	}

	var link string
	if info.Mode().Type()&fs.ModeSymlink != 0 {
		l, err := os.Readlink(f.Name())
		if err != nil {
			return err
		}

		link = l

		if !p.AllowAbsLink && filepath.IsAbs(link) {
			return fmt.Errorf("absolute link not allowed: %v -> %v", f.Name(), link)
		}
	}

	hdr, err := tar.FileInfoHeader(info, link)
	if err != nil {
		return err
	}

	hdr.Name = path

	return p.Write(f, hdr)
}

func (p *Packer) Write(r io.Reader, hdr *tar.Header) error {
	if err := p.tw.WriteHeader(hdr); err != nil {
		return err
	}

	if _, err := io.Copy(p.tw, io.LimitReader(r, hdr.Size)); err != nil {
		return err
	}

	return nil
}

func (p *Packer) Close() error {
	return p.tw.Close()
}
