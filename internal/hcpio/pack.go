package hcpio

import (
	"bytes"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"

	"github.com/hephbuild/heph/internal/hfs"
	"github.com/unikraft/go-cpio"
)

type Packer struct {
	cw           *cpio.Writer
	AllowAbsLink bool
}

func NewPacker(w io.Writer) *Packer {
	return &Packer{cw: cpio.NewWriter(w)}
}

func (p *Packer) WriteFile(f hfs.File, path string) error {
	info, err := f.LStat()
	if err != nil {
		return err
	}

	var link string
	if info.Mode()&fs.ModeSymlink != 0 {
		link, err = os.Readlink(f.Name())
		if err != nil {
			return err
		}

		if !p.AllowAbsLink && filepath.IsAbs(link) {
			return fmt.Errorf("absolute link not allowed: %v -> %v", f.Name(), link)
		}
	}

	hdr, err := cpio.FileInfoHeader(info, link)
	if err != nil {
		return err
	}

	hdr.Name = path

	if info.Mode()&fs.ModeSymlink != 0 {
		return p.Write(bytes.NewReader([]byte(hdr.Linkname)), hdr)
	}

	return p.Write(f, hdr)
}

func (p *Packer) Write(r io.Reader, hdr *cpio.Header) error {
	if err := p.cw.WriteHeader(hdr); err != nil {
		return err
	}

	if _, err := io.Copy(p.cw, io.LimitReader(r, hdr.Size)); err != nil {
		return err
	}

	return nil
}

func (p *Packer) Close() error {
	return p.cw.Close()
}
