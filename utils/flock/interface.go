package flock

import "context"

type Locker interface {
	Lock(ctx context.Context) error
	TryLock(ctx context.Context) (bool, error)
	Unlock() error
	Clean() error
}
