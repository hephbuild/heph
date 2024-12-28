package pluginbuildfile

import (
	"context"
	"github.com/hephbuild/hephv2/internal/hsingleflight"
	"go.starlark.net/starlark"
	"sync"
)

type CacheRunpkg struct {
	mu sync.RWMutex
	m  map[string]CacheRunpkgEntry
	sf hsingleflight.GroupMem[CacheRunpkgEntry]
}

type CacheRunpkgEntry struct {
	dict     starlark.StringDict
	payloads []OnTargetPayload
}

func (c *CacheRunpkg) key(pkg string) string {
	return pkg
}

func (c *CacheRunpkg) Singleflight(ctx context.Context, pkg string, onTarget onTargetFunc, f func(onTarget onTargetFunc) (starlark.StringDict, error)) (starlark.StringDict, error) {
	v, err, _ := c.sf.Do(c.key(pkg), func() (CacheRunpkgEntry, error) {
		var payloads []OnTargetPayload
		onTarget := func(ctx context.Context, payload OnTargetPayload) error {
			payloads = append(payloads, payload)

			return nil
		}

		dict, err := f(onTarget)
		if err != nil {
			return CacheRunpkgEntry{}, err
		}

		return CacheRunpkgEntry{
			dict:     dict,
			payloads: payloads,
		}, nil
	})

	for _, payload := range v.payloads {
		err := onTarget(ctx, payload)
		if err != nil {
			return nil, err
		}
	}

	return v.dict, err
}
