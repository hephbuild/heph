package xio

import (
	"io"
	"os"
	"time"
)

func Copy(dst io.Writer, src io.Reader, f func(written int64)) (int64, error) {
	return CopyBuffer(dst, src, nil, f)
}

func sizeGetterFactory(dst any) func() int64 {
	// vfs.File
	if x, ok := dst.(interface{ Size() (uint64, error) }); ok {
		return func() int64 {
			size, err := x.Size()
			if err != nil {
				return -1
			}

			return int64(size) // It's probably fine
		}
	}

	// *io.File
	if x, ok := dst.(interface{ Stat() (os.FileInfo, error) }); ok {
		return func() int64 {
			info, err := x.Stat()
			if err != nil {
				return -1
			}

			return info.Size()
		}
	}

	return nil
}

func doEvery(f func()) func() {
	t := time.NewTimer(100 * time.Millisecond)

	ch := make(chan struct{})

	go func() {
		defer func() {
			if !t.Stop() {
				<-t.C
			}
		}()

		for {
			select {
			case <-ch:
				return
			case <-t.C:
				f()
			}
		}
	}()

	return func() {
		close(ch)
	}
}

func CopyBuffer(dst io.Writer, src io.Reader, buf []byte, f func(written int64)) (int64, error) {
	// Optimizations from io.Copy
	if getSize := sizeGetterFactory(dst); getSize != nil {
		// If the reader has a WriteTo method, use it to do the copy.
		// Avoids an allocation and a copy.
		if wt, ok := src.(io.WriterTo); ok {
			stop := doEvery(func() {
				s := getSize()
				if s < 0 {
					return
				}
				f(s)
			})
			defer stop()

			return wt.WriteTo(dst)
		}
		// Similarly, if the writer has a ReadFrom method, use it to do the copy.
		if rt, ok := dst.(io.ReaderFrom); ok {
			stop := doEvery(func() {
				s := getSize()
				if s < 0 {
					return
				}
				f(s)
			})
			defer stop()

			return rt.ReadFrom(src)
		}
	}

	t := &Tracker{
		OnWrite: func(written int64) {
			f(written)
		},
	}

	w := io.MultiWriter(dst, t)

	written, err := io.CopyBuffer(w, src, buf)
	if err != nil {
		return written, err
	}

	if written == 0 {
		_, err := w.Write([]byte{})
		if err != nil {
			return written, err
		}
	}

	f(written)

	return written, nil
}
