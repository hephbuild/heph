package pluginbuildfile

import (
	"context"
	"fmt"
	"github.com/hephbuild/hephv2/internal/hsingleflight"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"sync"
)

type Cache struct {
	mu sync.RWMutex
	m  map[string]*pluginv1.TargetSpec
	sf hsingleflight.Group[*pluginv1.TargetSpec]
}

func (c *Cache) key(ref *pluginv1.TargetRef) string {
	return fmt.Sprintf("%s:%s", ref.Package, ref.Name)
}

func (c *Cache) Set(ctx context.Context, ref *pluginv1.TargetRef, spec *pluginv1.TargetSpec) {
	c.mu.Lock()
	defer c.mu.Unlock()

	if c.m == nil {
		c.m = map[string]*pluginv1.TargetSpec{}
	}

	c.m[c.key(ref)] = spec
}

func (c *Cache) Get(ctx context.Context, ref *pluginv1.TargetRef) (*pluginv1.TargetSpec, bool) {
	c.mu.RLock()
	defer c.mu.RUnlock()

	v, ok := c.m[c.key(ref)]

	return v, ok
}

func (c *Cache) Singleflight(ctx context.Context, ref *pluginv1.TargetRef, f func() (*pluginv1.TargetSpec, error)) (*pluginv1.TargetSpec, error) {
	v, err, _ := c.sf.Do(c.key(ref), f)
	return v, err
}
