package pluginbuildfile

import (
	"context"

	"github.com/hephbuild/heph/internal/hsingleflight"
	"go.starlark.net/starlark"
)

type CacheRunpkg struct {
	sf hsingleflight.GroupMemContext[string, CacheRunpkgEntry]
}

type CacheRunpkgEntry struct {
	dict           starlark.StringDict
	payloads       []OnTargetPayload
	providerStates []OnProviderStatePayload
}

func (c *CacheRunpkg) Singleflight(
	ctx context.Context,
	key string,
	hooks Hooks,
	f func(hooks Hooks) (starlark.StringDict, error),
) (starlark.StringDict, error) {
	v, err, _ := c.sf.Do(ctx, key, func(ctx context.Context) (CacheRunpkgEntry, error) {
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

		dict, err := f(Hooks{
			onTarget:        onTarget,
			onProviderState: onProviderState,
		})
		if err != nil {
			return CacheRunpkgEntry{}, err
		}

		return CacheRunpkgEntry{
			dict:           dict,
			payloads:       payloads,
			providerStates: providerStates,
		}, nil
	})
	if err != nil {
		return nil, err
	}

	if hooks.onTarget != nil {
		for _, payload := range v.payloads {
			err := hooks.onTarget(ctx, payload)
			if err != nil {
				return nil, err
			}
		}
	}

	if hooks.onProviderState != nil {
		for _, providerState := range v.providerStates {
			err := hooks.onProviderState(ctx, providerState)
			if err != nil {
				return nil, err
			}
		}
	}

	return v.dict, nil
}
