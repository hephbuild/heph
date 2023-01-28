package flock

import "context"

type Locker interface {
	Lock(ctx context.Context) error
	Unlock() error
	Clean() error
}
