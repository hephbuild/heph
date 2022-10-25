package utils

import (
	"fmt"
	log "github.com/sirupsen/logrus"
	"os"
	"path/filepath"
	"strconv"
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

	log.Debugf("Attempting to acquire lock for %s...", f.Name())
	err = syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB)
	if err != nil {
		l.m.Unlock()

		pid, err := os.ReadFile(f.Name())
		if err == nil && len(pid) > 0 {
			log.Warnf("Looks like process with PID %s has already acquired the lock for %s. Waiting for it to finish...", string(pid), f.Name())
		} else {
			log.Warnf("Looks like another process has already acquired the lock for %s. Waiting for it to finish...", f.Name())
		}

		if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX); err != nil {
			l.m.Lock()
			return fmt.Errorf("acquire lock for %s: %w", l.path, err)
		}
		l.m.Lock()
	}
	log.Debugf("Acquired lock for %s", f.Name())

	l.f = f

	if err := f.Truncate(0); err == nil {
		f.WriteAt([]byte(strconv.Itoa(os.Getpid())), 0)
	}

	return nil
}

func (l *Flock) Unlock() error {
	l.m.Lock()
	defer l.m.Unlock()

	f := l.f
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_UN); err != nil {
		return fmt.Errorf("release lock for %s: %s", l.path, err)
	}
	if err := f.Close(); err != nil {
		return fmt.Errorf("close lock file %s: %s", l.path, err)
	}

	f = nil

	return l.Clean()
}

func (l *Flock) Clean() error {
	err := os.RemoveAll(l.path)
	if err != nil {
		return err
	}

	return nil
}
