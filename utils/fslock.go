package utils

import (
	"errors"
	"fmt"
	"github.com/gofrs/flock"
	"os"
	"path/filepath"
	"sync"
)

type Locker interface {
	Lock() error
	Unlock() error
	Clean() error
}

func NewFlock(p string) *Flock {
	return &Flock{Flock: flock.New(p)}
}

type Flock struct {
	m sync.Mutex
	*flock.Flock
}

func (l *Flock) Lock() error {
	l.m.Lock()
	defer l.m.Unlock()

	if dir := filepath.Dir(l.Flock.Path()); dir != "." {
		err := os.MkdirAll(dir, os.ModePerm)
		if err != nil {
			return err
		}
	}

	err := l.Flock.Lock()
	if err != nil {
		return err
	}

	return nil
}

func (l *Flock) Unlock() error {
	l.m.Lock()
	defer l.m.Unlock()

	err := l.Flock.Unlock()
	if err != nil {
		return err
	}

	err = os.RemoveAll(l.Flock.Path())
	if err != nil {
		return fmt.Errorf("rm: %v", err)
	}

	return nil
}

func (l *Flock) Clean() error {
	err := os.RemoveAll(l.Path())
	if err != nil && errors.Is(err, os.ErrNotExist) {
		return err
	}

	return nil
}
