package engine

import (
	"context"
	"errors"
	"io"
)

var ErrCacheNotFound = errors.New("not found")

type Cache interface {
	Store(ctx context.Context, key string, r io.Reader) error
	Get(ctx context.Context, key string) (io.ReadCloser, error)
}

type CacheHas interface {
	Has(ctx context.Context, key string) (bool, error)
}
