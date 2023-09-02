package rcache

import (
	"context"
	"github.com/c2fo/vfs/v6/backend/gs"
	"github.com/c2fo/vfs/v6/backend/os"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/utils"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/hash"
	"golang.org/x/exp/slices"
	"io"
	"net/http"
	"time"
)

type orderCacheContainer struct {
	cache   CacheConfig
	latency time.Duration
}

func (c orderCacheContainer) calculateLatency(ctx context.Context) (time.Duration, error) {
	if c.cache.Location.FileSystem().Scheme() == os.Scheme {
		return -1, nil
	}

	if c.cache.Location.FileSystem().Scheme() == gs.Scheme {
		client := http.DefaultClient

		bucketName := c.cache.Location.Volume()
		url := "https://storage.googleapis.com/" + bucketName

		var mean time.Duration
		for i := 0; i < 10; i++ {
			start := time.Now()
			req, err := http.NewRequest("GET", url, nil)
			if err != nil {
				return -1, err
			}
			req = req.WithContext(ctx)

			res, err := client.Do(req)
			if err != nil {
				return -1, err
			}
			_, _ = io.Copy(io.Discard, res.Body)
			_ = res.Body.Close()

			duration := time.Since(start)
			mean += duration
			time.Sleep(time.Millisecond)
		}

		mean = mean / 10

		return mean, nil
	}

	return -1, nil
}

func orderCaches(ctx context.Context, caches []CacheConfig) []CacheConfig {
	if len(caches) <= 1 {
		return caches
	}

	orderedCaches := make([]orderCacheContainer, 0, len(caches))
	for _, cache := range caches {
		oc := orderCacheContainer{
			cache: cache,
		}

		var err error
		oc.latency, err = oc.calculateLatency(ctx)
		if err != nil {
			log.Errorf("latency: %v: skipping: %v", cache.Name, err)
		}

		orderedCaches = append(orderedCaches, oc)
	}

	slices.SortStableFunc(orderedCaches, func(a, b orderCacheContainer) int {
		if a.latency == b.latency {
			return 0
		}

		return int(a.latency - b.latency)
	})

	return ads.Map(orderedCaches, func(c orderCacheContainer) CacheConfig {
		return c.cache
	})
}

func (e *RemoteCache) OrderedCaches(ctx context.Context) ([]CacheConfig, error) {
	if len(e.Config.Caches) <= 1 || e.Config.CacheOrder != config.CacheOrderLatency {
		return e.Config.Caches, nil
	}

	if e.orderedCaches != nil {
		return e.orderedCaches, nil
	}

	err := e.orderedCachesLock.Lock(ctx)
	if err != nil {
		return nil, err
	}
	defer e.orderedCachesLock.Unlock()

	if e.orderedCaches != nil {
		return e.orderedCaches, nil
	}

	h := hash.NewHash()
	h.I64(1)
	hash.HashArray(h, e.Config.Caches, func(c CacheConfig) string {
		return c.Name + "|" + c.URI
	})

	cacheHash := h.Sum()
	cachePath := e.Root.Tmp.Join("caches_order").Abs()

	names, _ := utils.HashCache(cachePath, cacheHash, func() ([]string, error) {
		status.Emit(ctx, status.String("Measuring caches latency..."))
		ordered := orderCaches(ctx, e.Config.Caches)

		return ads.Map(ordered, func(c CacheConfig) string {
			return c.Name
		}), nil
	})

	cacheMap := map[string]CacheConfig{}
	for _, c := range e.Config.Caches {
		cacheMap[c.Name] = c
	}

	hasSecondary := false
	ordered := make([]CacheConfig, 0, len(names))
	for _, name := range names {
		c := cacheMap[name]
		if c.Secondary {
			// Use only one secondary
			if hasSecondary {
				continue
			}
			hasSecondary = true
		} else {
			// It's a primary cache, and its first, stop looking for other caches
			if len(ordered) == 0 {
				ordered = append(ordered, c)
				break
			}
		}
		ordered = append(ordered, c)
	}

	e.orderedCaches = ordered

	return ordered, nil
}
