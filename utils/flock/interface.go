package flock

type Locker interface {
	Lock() error
	Unlock() error
	Clean() error
}
