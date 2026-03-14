package engine

import (
	"context"
	"io"
	"iter"

	"github.com/hephbuild/heph/internal/hsync"
	"github.com/hephbuild/heph/lib/hkv"
	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

type SQLCacheKV interface {
	hkv.IO
	hkv.Lister
}

type SQLCache struct {
	kv    SQLCacheKV
	rpool hsync.Pool[[]byte]
}

func (c *SQLCache) Exists(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (bool, error) {
	key := c.targetKey(ref, hashin, name)

	return c.kv.Exists(ctx, key)
}

func (c *SQLCache) Delete(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) error {
	key := c.targetKey(ref, hashin, name)

	return c.kv.Delete(ctx, key)
}

var _ LocalCache = (*SQLCache)(nil)

func NewSQLCache(kv SQLCacheKV) *SQLCache {
	return &SQLCache{
		kv: kv,
	}
}

// targetKey returns the compound target address used as target_addr in the DB.
func (c *SQLCache) targetKey(ref *pluginv1.TargetRef, hashin, name string) string {
	return "cache " + tref.Format(ref) + " " + hashin + " " + name
}

func (c *SQLCache) Reader(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (io.ReadCloser, error) {
	key := c.targetKey(ref, hashin, name)

	r, ok, err := c.kv.Reader(ctx, key)
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, ErrLocalCacheNotFound
	}
	return r, nil
}

func (c *SQLCache) Writer(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (io.WriteCloser, error) {
	key := c.targetKey(ref, hashin, name)

	return c.kv.Writer(ctx, key, map[string]string{
		"target_pkg":    ref.GetPackage(),
		"target_addr":   tref.Format(ref),
		"hashin":        hashin,
		"artifact_name": name,
	}, 0)
}

func (c *SQLCache) ListArtifacts(ctx context.Context, ref *pluginv1.TargetRef, hashin string) iter.Seq2[string, error] {
	return func(yield func(string, error) bool) {
		for key, err := range c.kv.ListKeys(ctx, map[string]string{
			"target_addr": tref.Format(ref),
			"hashin":      hashin,
		}) {
			if err != nil {
				yield("", err)
				return
			}

			m, _, err := c.kv.GetMeta(ctx, key)
			if err != nil {
				yield("", err)
				return
			}
			if v, ok := m["artifact_name"]; ok {
				if !yield(v, nil) {
					return
				}
			}
		}
	}
}

func (c *SQLCache) ListVersions(ctx context.Context, ref *pluginv1.TargetRef) iter.Seq2[string, error] {
	return func(yield func(string, error) bool) {
		seen := map[string]struct{}{}
		for key, err := range c.kv.ListKeys(ctx, map[string]string{
			"target_addr": tref.Format(ref),
		}) {
			if err != nil {
				yield("", err)
				return
			}

			m, _, err := c.kv.GetMeta(ctx, key)
			if err != nil {
				yield("", err)
				return
			}
			if v, ok := m["hashin"]; ok {
				if _, ok := seen[v]; ok {
					continue
				}
				seen[v] = struct{}{}

				if !yield(v, nil) {
					return
				}
			}
		}
	}
}
