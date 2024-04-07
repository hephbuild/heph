package hash

import (
	"encoding/binary"
	"encoding/hex"
	"github.com/hephbuild/heph/utils/xsync"
	"github.com/zeebo/xxh3"
	"io"
)

type Hash interface {
	String(val string)
	I64(val int64)
	UI32(val uint32)
	Bool(val bool)

	Sum() string

	io.Writer
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

func (h *hasher) Write(p []byte) (int, error) {
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
