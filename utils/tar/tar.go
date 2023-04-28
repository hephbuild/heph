package tar

import (
	"archive/tar"
	"compress/gzip"
	"context"
	"encoding/json"
	"fmt"
	"github.com/hephbuild/heph/log/log"
	fs2 "github.com/hephbuild/heph/utils/fs"
	"github.com/hephbuild/heph/utils/sets"
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
	outTmp := out + ".tmp"

	tarf, err := os.Create(outTmp)
	if err != nil {
		return fmt.Errorf("tar: %w", err)
	}
	defer tarf.Close()

	go func() {
		<-ctx.Done()
		tarf.Close()
	}()

	gw := gzip.NewWriter(tarf)
	defer gw.Close()

	err = doTar(gw, files)
	if err != nil {
		return err
	}

	err = os.Rename(outTmp, out)
	if err != nil {
		return err
	}

	return nil
}

func doTar(w io.Writer, files []TarFile) error {
	tw := tar.NewWriter(w)
	defer tw.Close()

	for _, file := range files {
		info, err := os.Lstat(file.From)
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

func tarListFactory(tar string) (func(string), func() ([]string, error)) {
	files := make([]string, 0)
	recordFile := func(path string) {
		files = append(files, path)
	}

	return recordFile, func() ([]string, error) {
		listf, err := os.Create(tar + ".list")
		if err != nil {
			return nil, err
		}
		defer listf.Close()

		if err != nil {
			return nil, err
		}

		return files, json.NewEncoder(listf).Encode(files)
	}
}

type UntarOptions struct {
	List  bool
	RO    bool
	Dedup *sets.StringSet
}

func Untar(ctx context.Context, in, to string, list bool) (err error) {
	return UntarWith(ctx, in, to, UntarOptions{
		List: list,
	})
}

func UntarWith(ctx context.Context, in, to string, o UntarOptions) (err error) {
	log.Tracef("untar: %v to %v %#v", in, to, o)

	recordFile := func(string) {}
	if o.List {
		var complete func() ([]string, error)
		recordFile, complete = tarListFactory(in)

		defer func() {
			if err != nil {
				return
			}

			_, err = complete()
		}()
	}

	return Walk(ctx, in, func(hdr *tar.Header, tr *tar.Reader) error {
		dest := filepath.Join(to, hdr.Name)

		if o.Dedup != nil {
			if o.Dedup.Has(dest) {
				return nil
			}
			o.Dedup.Add(dest)
		}

		err := fs2.CreateParentDir(dest)
		if err != nil {
			return err
		}

		switch hdr.Typeflag {
		case tar.TypeReg:
			err = untarFile(hdr, tr, dest, o.RO)
			if err != nil {
				return fmt.Errorf("untar: %w", err)
			}

			recordFile(hdr.Name)
		case tar.TypeDir:
			err := os.MkdirAll(dest, os.FileMode(hdr.Mode))
			if err != nil {
				return fmt.Errorf("untar: %w", err)
			}
		case tar.TypeSymlink:
			if hdr.Linkname == "" {
				return fmt.Errorf("untar: symlink empty for %v", hdr.Name)
			}

			recordFile(hdr.Name)

			if fs2.PathExists(dest) {
				return nil
			}

			err := os.Symlink(hdr.Linkname, dest)
			if err != nil {
				return fmt.Errorf("untar: %w", err)
			}
		default:
			return fmt.Errorf("untar: unsupported type %v", hdr.Typeflag)
		}

		return nil
	})
}

func UntarList(ctx context.Context, in string) ([]string, error) {
	listPath := in + ".list"
	if fs2.PathExists(listPath) {
		f, err := os.Open(listPath)
		if err != nil {
			return nil, err
		}
		defer f.Close()

		var files []string
		err = json.NewDecoder(f).Decode(&files)
		if err == nil {
			return files, nil
		}
	}

	recordFile, complete := tarListFactory(in)

	err := Walk(ctx, in, func(hdr *tar.Header, tr *tar.Reader) error {
		switch hdr.Typeflag {
		case tar.TypeReg, tar.TypeSymlink:
			recordFile(hdr.Name)
		}

		return nil
	})
	if err != nil {
		return nil, err
	}

	return complete()
}

func Walk(ctx context.Context, path string, fs ...func(*tar.Header, *tar.Reader) error) error {
	tarf, err := os.Open(path)
	if err != nil {
		return fmt.Errorf("tarwalk: %w", err)
	}
	defer tarf.Close()

	go func() {
		<-ctx.Done()
		tarf.Close()
	}()

	gr, err := gzip.NewReader(tarf)
	if err != nil {
		return err
	}
	defer gr.Close()

	tr := tar.NewReader(gr)

	for {
		hdr, err := tr.Next()
		if err != nil {
			if err == io.EOF {
				break // End of archive
			}

			if ctx.Err() != nil {
				return ctx.Err()
			}

			return fmt.Errorf("untar: %w", err)
		}

		for _, f := range fs {
			err = f(hdr, tr)
			if err != nil {
				if ctx.Err() != nil {
					return ctx.Err()
				}

				return err
			}
		}
	}

	return nil
}

func untarFile(hdr *tar.Header, tr *tar.Reader, to string, ro bool) error {
	f, err := os.OpenFile(to, os.O_RDWR|os.O_CREATE|os.O_TRUNC, os.ModePerm)
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

	mode := os.FileMode(hdr.Mode)
	if ro {
		mode = mode &^ 0222
	}
	err = os.Chmod(to, mode)
	if err != nil {
		return err
	}

	err = os.Chtimes(to, hdr.AccessTime, hdr.ModTime)
	if err != nil {
		return err
	}

	return nil
}
