package utils

import (
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"syscall"
)

type Locker interface {
	Lock() error
	Unlock() error
	Clean() error
}

func NewFlock(p string) *Flock {
	return &Flock{path: p}
}

type Flock struct {
	m    sync.Mutex
	path string
	f    *os.File
}

func (l *Flock) Lock() error {
	l.m.Lock()
	defer l.m.Unlock()

	if dir := filepath.Dir(l.path); dir != "." {
		err := os.MkdirAll(dir, os.ModePerm)
		if err != nil {
			return err
		}
	}

	f, err := os.OpenFile(l.path, os.O_RDWR|os.O_CREATE, 0644)
	if err != nil {
		return fmt.Errorf("open %s to acquire lock: %w", l.path, err)
	}

	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX); err != nil {
		return fmt.Errorf("acquire lock for %s: %w", l.path, err)
	}

	l.f = f

	return nil
}

func (l *Flock) Unlock() error {
	l.m.Lock()
	defer l.m.Unlock()

	if err := syscall.Flock(int(l.f.Fd()), syscall.LOCK_UN); err != nil {
		return fmt.Errorf("release lock for %s: %s", l.path, err)
	}
	if err := l.f.Close(); err != nil {
		return fmt.Errorf("close lock file %s: %s", l.path, err)
	}

	l.f = nil

	return l.Clean()
}

func (l *Flock) Clean() error {
	err := os.RemoveAll(l.path)
	if err != nil {
		return err
	}

	return nil
}
