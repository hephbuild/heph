package utils

import (
	"fmt"
	"heph/log"
	"os"
	"path/filepath"
	"strconv"
	"strings"
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
	lm   sync.Mutex
	fm   sync.Mutex
	path string
	f    *os.File
}

func (l *Flock) Lock() error {
	l.lm.Lock()
	defer l.lm.Unlock()

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
		pid, err := os.ReadFile(f.Name())
		if err == nil && len(pid) > 0 {
			log.Warnf("Looks like process with PID %s has already acquired the lock for %s. Waiting for it to finish...", string(pid), f.Name())
		} else {
			log.Warnf("Looks like another process has already acquired the lock for %s. Waiting for it to finish...", f.Name())
		}

		if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX); err != nil {
			return fmt.Errorf("acquire lock for %s: %w", l.path, err)
		}
	}
	log.Debugf("Acquired lock for %s", f.Name())

	l.fm.Lock()
	defer l.fm.Unlock()

	l.f = f

	if err := f.Truncate(0); err == nil {
		f.WriteAt([]byte(strconv.Itoa(os.Getpid())), 0)
	}

	return nil
}

func (l *Flock) Unlock() error {
	l.fm.Lock()
	defer l.fm.Unlock()

	if l.f == nil {
		return nil
	}

	var errs []error
	if err := syscall.Flock(int(l.f.Fd()), syscall.LOCK_UN); err != nil {
		errs = append(errs, fmt.Errorf("release lock for %s: %s", l.path, err))
	}
	if err := l.f.Close(); err != nil {
		errs = append(errs, fmt.Errorf("close lock file %s: %s", l.path, err))
	}

	if err := l.Clean(); err != nil {
		errs = append(errs, fmt.Errorf("clean lock file for %v: %w", l.path, err))
	}

	l.f = nil

	errstr := make([]string, 0)
	for _, err := range errs {
		if err != nil {
			errstr = append(errstr, err.Error())
		}
	}

	if len(errs) > 0 {
		return fmt.Errorf("%v", strings.Join(errstr, ", "))
	}

	return nil
}

func (l *Flock) Clean() error {
	err := os.RemoveAll(l.path)
	if err != nil {
		return err
	}

	return nil
}
