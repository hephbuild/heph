package pluginbuildfile

import (
	"context"
	"fmt"

	"github.com/hephbuild/heph/internal/hsingleflight"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

type CacheGet struct {
	sf hsingleflight.GroupMem[*pluginv1.TargetSpec]
}

func (c *CacheGet) key(ref *pluginv1.TargetRef) string {
	return fmt.Sprintf("%s:%s", ref.GetPackage(), ref.GetName())
}

func (c *CacheGet) Singleflight(ctx context.Context, ref *pluginv1.TargetRef, f func() (*pluginv1.TargetSpec, error)) (*pluginv1.TargetSpec, error) {
	v, err, _ := c.sf.Do(c.key(ref), f)
	return v, err
}
