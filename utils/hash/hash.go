package hash

import (
	"encoding/binary"
	"encoding/hex"
	"github.com/hephbuild/heph/utils/xsync"
	"github.com/zeebo/xxh3"
	"io"
	"sort"
)

type Hash interface {
	String(val string)
	I64(val int64)
	UI32(val uint32)
	Bool(val bool)

	Sum() string

	io.Writer
}

func HashArray[T any](h Hash, a []T, f func(T) string) {
	entries := make([]string, 0, len(a))
	for _, e := range a {
		entries = append(entries, f(e))
	}

	sort.Strings(entries)

	for _, entry := range entries {
		h.String(entry)
	}
}

func HashMap[K comparable, V any](h Hash, a map[K]V, f func(K, V) string) {
	entries := make([]string, 0, len(a))
	for k, v := range a {
		entries = append(entries, f(k, v))
	}

	sort.Strings(entries)

	for _, entry := range entries {
		h.String(entry)
	}
}

func NewHash() Hash {
	h := &hasher{
		h: xxh3.New(),
	}

	return h
}

type hasher struct {
	h *xxh3.Hasher
}

func (h *hasher) Write(p []byte) (n int, err error) {
	return h.h.Write(p)
}

func (h *hasher) String(val string) {
	h.h.WriteString(val)
}

var buf8Pool = xsync.Pool[[]byte]{New: func() []byte {
	return make([]byte, 8)
}}

func (h *hasher) I64(val int64) {
	b := buf8Pool.Get()
	defer buf8Pool.Put(b)

	binary.BigEndian.PutUint64(b, uint64(val))
	h.Write(b)
}

func (h *hasher) UI32(val uint32) {
	b := buf8Pool.Get()
	defer buf8Pool.Put(b)

	binary.BigEndian.PutUint32(b, val)
	h.Write(b[:4])
}

var bytesTrue = []byte{1}
var bytesFalse = []byte{0}

func (h *hasher) Bool(val bool) {
	if val {
		h.Write(bytesTrue)
	} else {
		h.Write(bytesFalse)
	}
}

func (h *hasher) Sum() string {
	hb := h.h.Sum128().Bytes()
	return hex.EncodeToString(hb[:])
}

func HashString(s string) string {
	h := NewHash()
	h.String(s)
	return h.Sum()
}

func HashBytes(s []byte) string {
	h := NewHash()
	h.Write(s)
	return h.Sum()
}
