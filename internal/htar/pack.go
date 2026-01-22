package htar

import (
	"archive/tar"
	"fmt"
	"io"
	"os"
	"path/filepath"

	"github.com/hephbuild/heph/internal/hfs"
)

type Packer struct {
	tw *tar.Writer
}

func NewPacker(w io.Writer) *Packer {
	return &Packer{tw: tar.NewWriter(w)}
}

func (p *Packer) WriteFile(f hfs.File, path string) error {
	info, err := f.Stat()
	if err != nil {
		return err
	}

	var link string
	if info.Mode().Type() == os.ModeSymlink {
		l, err := os.Readlink(f.Name())
		if err != nil {
			return err
		}

		link = l

		if filepath.IsAbs(link) {
			return fmt.Errorf("absolute link not allowed: %v -> %v", f.Name(), link)
		}
	}

	hdr, err := tar.FileInfoHeader(info, link)
	if err != nil {
		return err
	}

	hdr.Name = path

	if err := p.tw.WriteHeader(hdr); err != nil {
		return err
	}

	if !info.Mode().IsRegular() { // nothing more to do for non-regular
		return nil
	}

	if _, err := io.Copy(p.tw, f); err != nil {
		return err
	}

	return nil
}

func (p *Packer) WriteSymlink(at, dst string) error {
	if err := p.tw.WriteHeader(&tar.Header{
		Typeflag: tar.TypeSymlink,
		Name:     at,
		Linkname: dst,
	}); err != nil {
		return err
	}

	return nil
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
