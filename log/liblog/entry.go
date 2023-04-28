package liblog

import (
	"fmt"
	"io"
	"time"
)

type FieldMarshal interface {
	Write(w io.Writer)
}

type FuncMarshal func(w io.Writer)

func (f FuncMarshal) Write(w io.Writer) {
	f(w)
}

var _ FieldMarshal = (FuncMarshal)(nil)

func StringMarshal(s string) FieldMarshal {
	return FuncMarshal(func(w io.Writer) {
		io.WriteString(w, s)
	})
}

func FallbackMarshal(v any) FieldMarshal {
	return FuncMarshal(func(w io.Writer) {
		io.WriteString(w, fmt.Sprint(v))
	})
}

type EntryField struct {
	Key   string
	Value FieldMarshal
}

type Entry struct {
	Timestamp time.Time
	Level     Level
	Message   string
	Fields    []EntryField
}
