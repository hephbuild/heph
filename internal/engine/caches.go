package engine

import engine2 "github.com/hephbuild/heph/lib/engine"

type CacheHandle struct {
	Name   string
	Client engine2.Cache
	Read   bool
	Write  bool
}

func (e *Engine) RegisterCache(name string, cache engine2.Cache, read, write bool) (CacheHandle, error) {
	h := CacheHandle{
		Name:   name,
		Client: cache,
		Read:   read,
		Write:  write,
	}

	e.Caches = append(e.Caches, h)

	return h, nil
}
