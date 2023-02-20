package flock

import (
	"context"
	"fmt"
	"golang.org/x/sys/unix"
	log "heph/hlog"
	"heph/utils/fs"
	"os"
	"strconv"
	"sync"
	"syscall"
	"time"
)

func NewFlock(name, p string) Locker {
	if name == "" || log.IsLevelEnabled(log.DebugLevel) {
		name = p
	}
	return &Flock{path: p, name: name}
}

type Flock struct {
	name string
	m    sync.Mutex
	path string
	f    *os.File
}

func (l *Flock) tryLock(onErr func(f *os.File) (bool, error)) (bool, error) {
	l.m.Lock()
	defer l.m.Unlock()

	err := fs.CreateParentDir(l.path)
	if err != nil {
		return false, err
	}

	f, err := os.OpenFile(l.path, os.O_RDWR|os.O_CREATE, 0644)
	if err != nil {
		return false, err
	}

	log.Debugf("Attempting to acquire lock for %s...", f.Name())
	err = syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB)
	if err != nil {
		if errno, _ := err.(unix.Errno); errno == unix.EWOULDBLOCK {
			ok, err := onErr(f)
			if !ok || err != nil {
				if err != nil {
					err = fmt.Errorf("acquire lock for %s: %w", l.name, err)
				}
				return false, err
			}
		} else {
			return false, err
		}
	}
	log.Debugf("Acquired lock for %s", f.Name())

	l.f = f

	if err := f.Truncate(0); err == nil {
		_, _ = f.WriteAt([]byte(strconv.Itoa(os.Getpid())), 0)
	}
	return true, nil
}

func (l *Flock) TryLock() (bool, error) {
	return l.tryLock(func(f *os.File) (bool, error) {
		return false, nil
	})
}

func (l *Flock) Lock(ctx context.Context) error {
	_, err := l.tryLock(func(f *os.File) (bool, error) {
		l.m.Unlock()
		defer l.m.Lock()

		doneCh := make(chan struct{})
		defer close(doneCh)

		pid, _ := os.ReadFile(f.Name())
		go func() {
			if fmt.Sprint(os.Getpid()) == string(pid) {
				log.Debugf("Looks another routine has already acquired the lock for %s. Waiting for it to finish...", l.name)
				return
			}

			select {
			case <-doneCh:
				// don't log
				return
			case <-time.After(500 * time.Millisecond):
				// log
			}

			if len(pid) > 0 {
				log.Warnf("Looks like process with PID %s has already acquired the lock for %s. Waiting for it to finish...", string(pid), l.name)
			} else {
				log.Warnf("Looks like another process has already acquired the lock for %s. Waiting for it to finish...", l.name)
			}
		}()

		lockCh := make(chan error, 1)

		go func() {
			// This will block forever if the ctx completes before
			lockCh <- syscall.Flock(int(f.Fd()), syscall.LOCK_EX)
		}()

		select {
		case err := <-lockCh:
			if err != nil {
				return false, err
			}
		case <-ctx.Done():
			return false, ctx.Err()
		}

		return true, nil
	})
	return err
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

	return nil
}

func (l *Flock) Clean() error {
	err := os.RemoveAll(l.path)
	if err != nil {
		return err
	}

	return nil
}
