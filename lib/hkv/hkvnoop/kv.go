package hkvnoop

import (
	"context"
	"time"

	"github.com/hephbuild/heph/lib/hkv"
)

type kv struct{}

func New() hkv.Bytes {
	return kv{}
}

func (p kv) Get(ctx context.Context, key string) ([]byte, map[string]string, bool, error) {
	return nil, nil, false, nil
}

func (p kv) Exists(ctx context.Context, key string) (bool, error) {
	return false, nil
}

func (p kv) Set(ctx context.Context, key string, value []byte, metadata map[string]string, ttl time.Duration) error {
	return nil
}

func (p kv) Delete(ctx context.Context, key string) error {
	return nil
}
