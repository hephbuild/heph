package engine

import (
	"github.com/hephbuild/heph/lib/pluginsdk"
)

type CacheHandle struct {
	Name   string
	Client pluginsdk.Cache
	Read   bool
	Write  bool
}

func (e *Engine) RegisterCache(name string, cache pluginsdk.Cache, read, write bool) (CacheHandle, error) {
	h := CacheHandle{
		Name:   name,
		Client: cache,
		Read:   read,
		Write:  write,
	}

	e.Caches = append(e.Caches, h)

	return h, nil
}
