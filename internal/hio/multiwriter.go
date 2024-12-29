package hio

import (
	"io"
	"slices"
)

func MultiWriter(writers ...io.Writer) io.Writer {
	writers = slices.DeleteFunc(writers, func(w io.Writer) bool {
		return w == nil
	})

	switch len(writers) {
	case 0:
		return nil
	case 1:
		return writers[0]
	}

	return io.MultiWriter(writers...)
}
