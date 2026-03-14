package hkv

import (
	"context"
	"fmt"
	"time"

	"github.com/hephbuild/heph/internal/hproto"
	"github.com/hephbuild/heph/lib/hkv"
	"google.golang.org/protobuf/proto"
)

type kv[V proto.Message] struct {
	inner hkv.Bytes
}

func New[V proto.Message](inner hkv.Bytes) hkv.KV[V] {
	return &kv[V]{
		inner: inner,
	}
}

func (p *kv[V]) Get(ctx context.Context, key string) (V, map[string]string, bool, error) {
	var zero V
	data, meta, ok, err := p.inner.Get(ctx, key)
	if err != nil || !ok {
		return zero, meta, ok, err
	}

	val := hproto.New[V]()
	err = proto.Unmarshal(data, val)
	if err != nil {
		return zero, nil, false, fmt.Errorf("unmarshal: %w", err)
	}

	return val, meta, true, nil
}

func (p *kv[V]) Exists(ctx context.Context, key string) (bool, error) {
	return p.inner.Exists(ctx, key)
}

func (p *kv[V]) Set(ctx context.Context, key string, value V, metadata map[string]string, ttl time.Duration) error {
	data, err := proto.Marshal(value)
	if err != nil {
		return fmt.Errorf("marshal: %w", err)
	}

	return p.inner.Set(ctx, key, data, metadata, ttl)
}

func (p *kv[V]) Delete(ctx context.Context, key string) error {
	return p.inner.Delete(ctx, key)
}
