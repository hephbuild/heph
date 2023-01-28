package flock

import "context"

type Locker interface {
	Lock(ctx context.Context) error
	TryLock() (bool, error)
	Unlock() error
	Clean() error
}
