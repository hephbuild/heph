package tar

import (
	"archive/tar"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/utils/xio"
	"github.com/hephbuild/heph/utils/xprogress"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"time"
)

type File struct {
	From string
	To   string
}

func tarWriteEntry(file File, tw *tar.Writer, info os.FileInfo) error {
	var link string
	if info.Mode().Type() == os.ModeSymlink {
		l, err := os.Readlink(file.From)
		if err != nil {
			return err
		}

		link = l

		if filepath.IsAbs(link) {
			return fmt.Errorf("absolute link not allowed: %v -> %v", file.From, link)
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

func tarWriteDir(file File, tw *tar.Writer) error {
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

		return tarWriteEntry(File{
			From: path,
			To:   filepath.Join(file.To, rel),
		}, tw, info)
	})
}

func Tar(w io.Writer, files []File) error {
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

func tarListFactory(path string) (func(string), func() ([]string, error)) {
	files := make([]string, 0)
	recordFile := func(path string) {
		files = append(files, path)
	}

	return recordFile, func() ([]string, error) {
		listf, err := os.Create(path)
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
	ListPath string
	RO       bool
	Dedup    *sets.StringSet
	Progress func(written int64)
}

func UntarPath(ctx context.Context, in, to string, o UntarOptions) (err error) {
	tarf, err := os.Open(in)
	if err != nil {
		return fmt.Errorf("tarwalk: %w", err)
	}
	defer tarf.Close()

	return UntarContext(ctx, tarf, to, o)
}

func UntarContext(ctx context.Context, in io.ReadCloser, to string, o UntarOptions) (err error) {
	cancel := xio.ContextCloser(ctx, in)
	defer cancel()

	return Untar(in, to, o)
}

// See https://unix.stackexchange.com/a/557487
const HeaderOverhead = 512

func Untar(in io.Reader, to string, o UntarOptions) (err error) {
	recordFile := func(string) {}
	if o.ListPath != "" {
		var complete func() ([]string, error)
		recordFile, complete = tarListFactory(o.ListPath)

		defer func() {
			if err != nil {
				return
			}

			_, err = complete()
		}()
	}

	if o.Progress != nil {
		u := xprogress.NewCounter()
		pin := xprogress.NewReaderTap(in, u)
		in = pin

		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()

		go func() {
			progressChan := xprogress.NewTicker(ctx, u, 10*time.Millisecond)
			for p := range progressChan {
				o.Progress(p.N())
			}
		}()
	}

	return Walk(in, func(hdr *tar.Header, tr *tar.Reader) error {
		dest := filepath.Join(to, hdr.Name)

		if o.Dedup != nil {
			if o.Dedup.Has(dest) {
				return nil
			}
			o.Dedup.Add(dest)
		}

		err := xfs.CreateParentDir(dest)
		if err != nil {
			return err
		}

		switch hdr.Typeflag {
		case tar.TypeReg:
			err = untarFile(hdr, tr, dest, o.RO)
			if err != nil {
				return fmt.Errorf("untar: %v: %w", hdr.Name, err)
			}

			recordFile(hdr.Name)
		case tar.TypeDir:
			err := os.MkdirAll(dest, os.FileMode(hdr.Mode))
			if err != nil {
				return fmt.Errorf("untar: %v: %w", hdr.Name, err)
			}
		case tar.TypeSymlink:
			if hdr.Linkname == "" {
				return fmt.Errorf("untar: symlink empty for %v", hdr.Name)
			}

			recordFile(hdr.Name)

			if xfs.PathExists(dest) {
				return nil
			}

			err := os.Symlink(hdr.Linkname, dest)
			if err != nil {
				return fmt.Errorf("untar: %v: %w", hdr.Name, err)
			}
		default:
			return fmt.Errorf("untar: unsupported type %v", hdr.Typeflag)
		}

		return nil
	})
}

func UntarList(ctx context.Context, in io.ReadCloser, listPath string, progresss func(read int64)) ([]string, error) {
	if xfs.PathExists(listPath) {
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

	recordFile, complete := tarListFactory(listPath)

	cancel := xio.ContextCloser(ctx, in)
	defer cancel()

	c := xprogress.NewCounter()
	if progresss != nil {
		ctx, cancel := context.WithCancel(ctx)
		defer cancel()

		go func() {
			progressChan := xprogress.NewTicker(ctx, c, 10*time.Millisecond)
			for p := range progressChan {
				progresss(p.N())
			}
		}()
	}

	err := Walk(xprogress.NewReaderTap(in, c), func(hdr *tar.Header, tr *tar.Reader) error {
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

func WalkPath(ctx context.Context, path string, fs ...func(*tar.Header, *tar.Reader) error) error {
	tarf, err := os.Open(path)
	if err != nil {
		return fmt.Errorf("tarwalk: %w", err)
	}
	defer tarf.Close()

	cancel := xio.ContextCloser(ctx, tarf)
	defer cancel()

	return Walk(tarf, fs...)
}

func Walk(tarf io.Reader, fs ...func(*tar.Header, *tar.Reader) error) error {
	tr := tar.NewReader(tarf)

	for {
		hdr, err := tr.Next()
		if err != nil {
			if err == io.EOF {
				break // End of archive
			}

			return fmt.Errorf("walk: %w", err)
		}

		for _, f := range fs {
			err = f(hdr, tr)
			if err != nil {
				return err
			}
		}
	}

	return nil
}

func untarFile(hdr *tar.Header, tr *tar.Reader, to string, ro bool) error {
	info, err := os.Lstat(to)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}

	if info != nil {
		// The file is probably not changed... This should prevent an infinite loop of
		// a file being codegen copy_noexclude & input to another target
		if hdr.Size == info.Size() && hdr.ModTime == info.ModTime() {
			return nil
		}
	}

	f, err := os.OpenFile(to, os.O_RDWR|os.O_CREATE|os.O_TRUNC, os.ModePerm)
	if err != nil {
		return err
	}
	defer f.Close()

	_, err = io.CopyN(f, tr, hdr.Size)
	if err != nil {
		return err
	}

	err = xfs.CloseEnsureROFD(f)
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
