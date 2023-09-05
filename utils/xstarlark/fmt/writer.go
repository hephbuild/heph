package fmt

import (
	"fmt"
	"strings"
)

type Builder struct {
	strings.Builder
	lineLength int
}

func (w *Builder) WriteStringRaw(s string) {
	w.WriteString(s)
}

func (w *Builder) WriteString(s string) {
	if s == "" {
		return
	}

	for _, c := range s {
		if c == '\n' {
			w.lineLength = 0
		} else {
			w.lineLength++
		}
	}

	w.Builder.WriteString(s)
}

func (w *Builder) LineLength() int {
	return w.lineLength
}

type Writer interface {
	fmt.Stringer
	WriteString(s string)
	WriteStringRaw(s string)
	Len() int
	LineLength() int
}

type indentWriter struct {
	w           Writer
	i           string
	n           int
	hasIndented bool
}

func (w *indentWriter) LineLength() int {
	return w.w.LineLength()
}

func (w *indentWriter) String() string {
	return w.w.String()
}

func (w *indentWriter) WriteStringRaw(s string) {
	first, rest, ok := strings.Cut(s, "\n")
	if !ok {
		w.WriteString(s)
		return
	}

	w.WriteString(first + "\n")
	rootWriter(w).WriteString(rest)
	walkIndentWriter(w, func(w *indentWriter) {
		w.hasIndented = true
	})
}

func (w *indentWriter) WriteString(s string) {
	i := strings.Repeat(w.i, w.n)

	writer := w.w.WriteString

	for _, c := range s {
		if !w.hasIndented && c != '\n' {
			writer(i)
			w.hasIndented = true
		}

		writer(string(c))

		if c == '\n' {
			w.hasIndented = false
		}
	}
}

func (w *indentWriter) Len() int {
	return w.w.Len()
}

func rootWriter(w Writer) Writer {
	ww := w
	for {
		uw, ok := ww.(*indentWriter)
		if !ok {
			break
		}

		ww = uw.w
	}

	return ww
}

func walkIndentWriter(w Writer, f func(w *indentWriter)) {
	ww := w
	for {
		uw, ok := ww.(*indentWriter)
		if !ok {
			break
		}

		f(uw)

		ww = uw.w
	}
}

func isTopLevel(w Writer) bool {
	_, ok := w.(*indentWriter)
	if ok {
		return false
	}

	return true
}
