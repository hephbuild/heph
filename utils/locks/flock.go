package locks

import (
	"context"
	"fmt"
	log "github.com/hephbuild/heph/log/liblog"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/utils/xfs"
	"golang.org/x/sys/unix"
	"os"
	"strconv"
	"sync"
	"syscall"
	"time"
)

func NewFlock(name, p string) RWLocker {
	if name == "" || log.Default().IsLevelEnabled(log.DebugLevel) {
		name = p
	}
	return &Flock{path: p, name: name}
}

type Flock struct {
	name string
	m    sync.RWMutex
	path string
	f    *os.File
}

// if we use os.File.Fd(), the file is set to blocking mode, preventing the fd to be closed on failure to acquire lock
func fileDoFd(f *os.File, fun func(fd uintptr) error) error {
	rawConn, err := f.SyscallConn()
	if err != nil {
		return err
	}

	var rerr error
	werr := rawConn.Write(func(fd uintptr) (done bool) {
		rerr = fun(fd)
		return true
	})
	if werr != nil {
		return werr
	}
	return rerr
}

func (l *Flock) tryLock(ctx context.Context, ro bool, onErr func(f *os.File, how int) (bool, error)) (bool, error) {
	logger := log.FromContext(ctx)

	fhow := syscall.O_RDWR
	lhow := syscall.LOCK_EX
	if ro {
		fhow = syscall.O_RDONLY
		lhow = syscall.LOCK_SH

		l.m.RLock()
		defer l.m.RUnlock()
	} else {
		l.m.Lock()
		defer l.m.Unlock()
	}

	err := xfs.CreateParentDir(l.path)
	if err != nil {
		return false, err
	}

	f, err := os.OpenFile(l.path, fhow|os.O_CREATE, 0644)
	if err != nil {
		return false, err
	}
	defer func() {
		if f != l.f {
			f.Close()
		}
	}()

	logger.Debugf("Attempting to acquire lock for %s...", f.Name())
	err = fileDoFd(f, func(fd uintptr) error {
		return syscall.Flock(int(fd), lhow|syscall.LOCK_NB)
	})
	if err != nil {
		if errno, _ := err.(unix.Errno); errno == unix.EWOULDBLOCK {
			ok, err := onErr(f, lhow)
			if err != nil {
				return false, fmt.Errorf("acquire lock for %s: %w", l.name, err)
			}

			if !ok {
				logger.Debugf("Failed to acquire lock for %s", f.Name())

				return false, nil
			}
		} else {
			return false, err
		}
	}
	logger.Debugf("Acquired lock for %s", f.Name())

	l.f = f

	if !ro {
		if err := f.Truncate(0); err == nil {
			_, _ = f.WriteAt([]byte(strconv.Itoa(os.Getpid())), 0)
		}
	}

	return true, nil
}

func (l *Flock) lock(ctx context.Context, ro bool) error {
	_, err := l.tryLock(ctx, ro, func(f *os.File, how int) (bool, error) {
		l.m.Unlock()
		defer l.m.Lock()

		doneCh := make(chan struct{})
		defer close(doneCh)

		pidb, _ := os.ReadFile(f.Name())
		pid := string(pidb)
		go func() {
			select {
			case <-doneCh:
				// don't log
				return
			case <-time.After(500 * time.Millisecond):
				// log
			}

			if len(pid) > 0 {
				if strconv.Itoa(os.Getpid()) == pid {
					status.Emit(ctx, status.String(fmt.Sprintf("Another job locked %v, waiting...", l.name)))
				} else {
					status.Emit(ctx, status.String(fmt.Sprintf("Process %v locked %v, waiting...", pid, l.name)))
				}
			} else {
				status.Emit(ctx, status.String(fmt.Sprintf("Another process locked %v, waiting...", l.name)))
			}
		}()

		lockCh := make(chan error, 1)

		go func() {
			defer close(lockCh)

			err := fileDoFd(f, func(fd uintptr) error {
				lockCh <- syscall.Flock(int(fd), how)
				return nil
			})
			if err != nil {
				lockCh <- err
			}
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

func (l *Flock) TryLock(ctx context.Context) (bool, error) {
	return l.tryLock(ctx, false, func(f *os.File, how int) (bool, error) {
		return false, nil
	})
}

func (l *Flock) Lock(ctx context.Context) error {
	return l.lock(ctx, false)
}

func (l *Flock) Unlock() error {
	l.m.Lock()
	defer l.m.Unlock()

	f := l.f

	// Try to wipe the pid if we have write perm
	_ = f.Truncate(0)

	if err := fileDoFd(f, func(fd uintptr) error {
		return syscall.Flock(int(fd), syscall.LOCK_UN)
	}); err != nil {
		return fmt.Errorf("release lock for %s: %s", l.path, err)
	}
	if err := f.Close(); err != nil {
		return fmt.Errorf("close lock file %s: %s", l.path, err)
	}

	f = nil

	return nil
}

func (l *Flock) TryRLock(ctx context.Context) (bool, error) {
	return l.tryLock(ctx, true, func(f *os.File, how int) (bool, error) {
		return false, nil
	})
}

func (l *Flock) RLock(ctx context.Context) error {
	return l.lock(ctx, true)
}

func (l *Flock) RUnlock() error {
	return l.Unlock()
}

func (l *Flock) Clean() error {
	err := os.RemoveAll(l.path)
	if err != nil {
		return err
	}

	return nil
}
