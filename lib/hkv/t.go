package hkv

import (
	"context"
	"fmt"
	"time"
)

func NewT[T any](kv Bytes, c Codec[T]) KV[T] {
	return &kvt[T]{
		Bytes: kv,
		c:     c,
	}
}

type kvt[V any] struct {
	Bytes
	c Codec[V]
}

func (p *kvt[V]) Get(ctx context.Context, key string) (V, map[string]string, bool, error) {
	data, meta, ok, err := p.Bytes.Get(ctx, key)
	if err != nil || !ok {
		var zero V
		return zero, meta, ok, err
	}

	val, err := p.c.Unmarshal(data)
	if err != nil {
		var zero V
		return zero, nil, false, fmt.Errorf("unmarshal: %w", err)
	}

	return val, meta, true, nil
}

func (p *kvt[V]) Set(ctx context.Context, key string, value V, metadata map[string]string, ttl time.Duration) error {
	data, err := p.c.Marshal(value)
	if err != nil {
		return fmt.Errorf("marshal: %w", err)
	}

	return p.Bytes.Set(ctx, key, data, metadata, ttl)
}
