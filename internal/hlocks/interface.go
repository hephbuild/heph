package hlocks

import "context"

type Unlocker interface {
	Unlock() error
}

type Locker interface {
	Lock(ctx context.Context) error
	TryLock(ctx context.Context) (bool, error)
	Unlock() error
	Clean() error
}

type RWLocker interface {
	Locker
	RLock(ctx context.Context) error
	TryRLock(ctx context.Context) (bool, error)
	RUnlock() error
}
