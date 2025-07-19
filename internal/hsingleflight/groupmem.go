package hsingleflight

import "context"

type GroupMem[K comparable, T any] struct {
	GroupMemContext[K, T]
}

func (g *GroupMem[K, T]) Do(key K, do func() (T, error)) (T, error, bool) {
	return g.GroupMemContext.Do(context.Background(), key, func(ctx context.Context) (T, error) {
		return do()
	})
}
