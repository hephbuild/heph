package hkvproto

import (
	"fmt"

	"github.com/hephbuild/heph/internal/hproto"
	"github.com/hephbuild/heph/lib/hkv"
	"google.golang.org/protobuf/proto"
)

type Codec[T proto.Message] struct {
}

func New[T proto.Message]() hkv.Codec[T] {
	return &Codec[T]{}
}

func (p *Codec[T]) Marshal(value T) ([]byte, error) {
	return proto.Marshal(value)
}
func (p *Codec[T]) Unmarshal(data []byte) (T, error) {
	if len(data) == 0 {
		var zero T
		return zero, nil
	}

	val := hproto.New[T]()
	err := proto.Unmarshal(data, val)
	if err != nil {
		var zero T
		return zero, fmt.Errorf("unmarshal: %w", err)
	}

	return val, nil
}
