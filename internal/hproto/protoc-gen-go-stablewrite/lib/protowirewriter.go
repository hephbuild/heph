package lib

import "io"

// copied from google.golang.org/protobuf@v1.36.6/encoding/protowire/wire.go

func WriteVarint(w io.Writer, v uint64) (n int, err error) { //nolint:nonamedreturns
	switch {
	case v < 1<<7:
		return w.Write([]byte{byte(v)})
	case v < 1<<14:
		return w.Write([]byte{
			byte((v>>0)&0x7f | 0x80),
			byte(v >> 7),
		})
	case v < 1<<21:
		return w.Write([]byte{
			byte((v>>0)&0x7f | 0x80),
			byte((v>>7)&0x7f | 0x80),
			byte(v >> 14),
		})
	case v < 1<<28:
		return w.Write([]byte{
			byte((v>>0)&0x7f | 0x80),
			byte((v>>7)&0x7f | 0x80),
			byte((v>>14)&0x7f | 0x80),
			byte(v >> 21),
		})
	case v < 1<<35:
		return w.Write([]byte{
			byte((v>>0)&0x7f | 0x80),
			byte((v>>7)&0x7f | 0x80),
			byte((v>>14)&0x7f | 0x80),
			byte((v>>21)&0x7f | 0x80),
			byte(v >> 28),
		})
	case v < 1<<42:
		return w.Write([]byte{
			byte((v>>0)&0x7f | 0x80),
			byte((v>>7)&0x7f | 0x80),
			byte((v>>14)&0x7f | 0x80),
			byte((v>>21)&0x7f | 0x80),
			byte((v>>28)&0x7f | 0x80),
			byte(v >> 35),
		})
	case v < 1<<49:
		return w.Write([]byte{
			byte((v>>0)&0x7f | 0x80),
			byte((v>>7)&0x7f | 0x80),
			byte((v>>14)&0x7f | 0x80),
			byte((v>>21)&0x7f | 0x80),
			byte((v>>28)&0x7f | 0x80),
			byte((v>>35)&0x7f | 0x80),
			byte(v >> 42),
		})
	case v < 1<<56:
		return w.Write([]byte{
			byte((v>>0)&0x7f | 0x80),
			byte((v>>7)&0x7f | 0x80),
			byte((v>>14)&0x7f | 0x80),
			byte((v>>21)&0x7f | 0x80),
			byte((v>>28)&0x7f | 0x80),
			byte((v>>35)&0x7f | 0x80),
			byte((v>>42)&0x7f | 0x80),
			byte(v >> 49),
		})
	case v < 1<<63:
		return w.Write([]byte{
			byte((v>>0)&0x7f | 0x80),
			byte((v>>7)&0x7f | 0x80),
			byte((v>>14)&0x7f | 0x80),
			byte((v>>21)&0x7f | 0x80),
			byte((v>>28)&0x7f | 0x80),
			byte((v>>35)&0x7f | 0x80),
			byte((v>>42)&0x7f | 0x80),
			byte((v>>49)&0x7f | 0x80),
			byte(v >> 56),
		})
	default:
		return w.Write([]byte{
			byte((v>>0)&0x7f | 0x80),
			byte((v>>7)&0x7f | 0x80),
			byte((v>>14)&0x7f | 0x80),
			byte((v>>21)&0x7f | 0x80),
			byte((v>>28)&0x7f | 0x80),
			byte((v>>35)&0x7f | 0x80),
			byte((v>>42)&0x7f | 0x80),
			byte((v>>49)&0x7f | 0x80),
			byte((v>>56)&0x7f | 0x80),
			1,
		})
	}
}
