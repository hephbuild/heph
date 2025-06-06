package pluginbuildfile

import (
	"context"

	"github.com/hephbuild/heph/internal/hsingleflight"
	"go.starlark.net/starlark"
)

type CacheRunpkg struct {
	sf hsingleflight.GroupMem[string, CacheRunpkgEntry]
}

type CacheRunpkgEntry struct {
	dict           starlark.StringDict
	payloads       []OnTargetPayload
	providerStates []OnProviderStatePayload
}

func (c *CacheRunpkg) key(pkg string) string {
	return pkg
}

func (c *CacheRunpkg) Singleflight(
	ctx context.Context,
	pkg string,
	onTarget onTargetFunc,
	onProviderState onProviderStateFunc,
	f func(onTarget onTargetFunc, onProviderState onProviderStateFunc) (starlark.StringDict, error),
) (starlark.StringDict, error) {
	v, err, _ := c.sf.Do(c.key(pkg), func() (CacheRunpkgEntry, error) {
		var payloads []OnTargetPayload
		onTarget := func(ctx context.Context, payload OnTargetPayload) error {
			payloads = append(payloads, payload)

			return nil
		}

		var providerStates []OnProviderStatePayload
		onProviderState := func(ctx context.Context, payload OnProviderStatePayload) error {
			providerStates = append(providerStates, payload)

			return nil
		}

		dict, err := f(onTarget, onProviderState)
		if err != nil {
			return CacheRunpkgEntry{}, err
		}

		return CacheRunpkgEntry{
			dict:           dict,
			payloads:       payloads,
			providerStates: providerStates,
		}, nil
	})

	if onTarget != nil {
		for _, payload := range v.payloads {
			err := onTarget(ctx, payload)
			if err != nil {
				return nil, err
			}
		}
	}

	if onProviderState != nil {
		for _, providerState := range v.providerStates {
			err := onProviderState(ctx, providerState)
			if err != nil {
				return nil, err
			}
		}
	}

	return v.dict, err
}
