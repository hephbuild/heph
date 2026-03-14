package hkv

import (
	"context"
	"io"
	"iter"
	"time"
)

type Bytes = KV[[]byte]

type KV[V any] interface {
	Get(ctx context.Context, key string) (V, map[string]string, bool, error)
	Exists(ctx context.Context, key string) (bool, error)
	Set(ctx context.Context, key string, value V, metadata map[string]string, ttl time.Duration) error
	Delete(ctx context.Context, key string) error
}

type IO interface {
	Reader(ctx context.Context, key string) (io.ReadCloser, bool, error)
	Exists(ctx context.Context, key string) (bool, error)
	Writer(ctx context.Context, key string, metadata map[string]string, ttl time.Duration) (io.WriteCloser, error)
	Delete(ctx context.Context, key string) error
}

type Lister interface {
	ListKeys(ctx context.Context, query map[string]string) iter.Seq2[string, error]
	GetMeta(ctx context.Context, key string) (map[string]string, bool, error)
}

type Codec[T any] interface {
	Marshal(value T) ([]byte, error)
	Unmarshal(data []byte) (T, error)
}
