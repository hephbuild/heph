package hlocks

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"sync"
	"syscall"
	"time"

	"github.com/hephbuild/heph/internal/flock"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hfs"
)

func NewFlock2(fs hfs.OS, name, path string, allowCreate bool) *Flock {
	if name == "" {
		name = path
	}
	return &Flock{fs: fs, path: path, name: name, allowCreate: allowCreate}
}

func NewFlock(fs hfs.OS, name, path string) *Flock {
	return NewFlock2(fs, name, path, true)
}

type Flock struct {
	name        string
	m           sync.RWMutex
	path        string
	f           *os.File
	fs          hfs.OS
	allowCreate bool
}

func (l *Flock) tryLock(ctx context.Context, ro bool, onErr func(f *os.File, ro bool) (bool, error)) (bool, error) {
	fhow := syscall.O_RDWR
	if ro {
		fhow = syscall.O_RDONLY

		l.m.RLock()
		defer l.m.RUnlock()
	} else {
		l.m.Lock()
		defer l.m.Unlock()
	}

	err := l.fs.MkdirAll(filepath.Dir(l.path), hfs.ModePerm)
	if err != nil {
		return false, err
	}

	if l.allowCreate {
		fhow |= os.O_CREATE
	}

	hf, err := l.fs.Open(l.path, fhow, 0644)
	if err != nil {
		return false, err
	}

	f := hf.(*os.File) //nolint:errcheck

	defer func() {
		if f != l.f {
			_ = f.Close()
		}
	}()

	err = flock.Flock(f, ro, false)
	if err != nil { //nolint:nestif
		if flock.IsErrWouldBlock(err) {
			ok, err := onErr(f, ro)
			if err != nil {
				return false, fmt.Errorf("acquire lock for %s: %w", l.name, err)
			}

			if !ok {
				return false, nil
			}
		} else {
			return false, err
		}
	}

	l.f = f

	if !ro && l.allowCreate {
		if err := f.Truncate(0); err == nil {
			_, _ = f.WriteAt([]byte(strconv.Itoa(os.Getpid())), 0)
		}
	}

	return true, nil
}

func (l *Flock) lock(ctx context.Context, ro bool) error {
	_, err := l.tryLock(ctx, ro, func(f *os.File, ro bool) (bool, error) {
		if ro {
			l.m.RUnlock()
			defer l.m.RLock()
		} else {
			l.m.Unlock()
			defer l.m.Lock()
		}

		doneCh := make(chan struct{})
		defer close(doneCh)

		go func() {
			select {
			case <-doneCh:
				// don't log
				return
			case <-time.After(500 * time.Millisecond):
				// log
			}

			for {
				select {
				case <-doneCh:
					break
				default:
				}

				var pidStr string
				if l.allowCreate {
					pidb, _ := hfs.ReadFile(l.fs, f.Name())
					pidStr = string(pidb)
				}

				if len(pidStr) > 0 {
					pid, err := strconv.Atoi(pidStr)
					if err != nil {
						pid = -1
					}

					if os.Getpid() == pid {
						hlog.From(ctx).Debug(fmt.Sprintf("Another job locked %v, waiting...", l.name))
					} else {
						processDetails := getProcessDetails(pid)

						hlog.From(ctx).Debug(fmt.Sprintf("Process %v locked %v, waiting...", processDetails, l.name))
					}

					break
				} else {
					hlog.From(ctx).Debug(fmt.Sprintf("Another process locked %v, waiting...", l.name))
					time.Sleep(time.Second)
				}
			}
		}()

		lockCh := make(chan error, 1)

		go func() {
			defer close(lockCh)
			lockCh <- flock.Flock(f, ro, true)
		}()

		select {
		case <-ctx.Done():
			return false, ctx.Err()
		case err := <-lockCh:
			if err != nil {
				return false, err
			}
		}

		return true, nil
	})

	return err
}

func (l *Flock) TryLock(ctx context.Context) (bool, error) {
	return l.tryLock(ctx, false, func(f *os.File, ro bool) (bool, error) {
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

	if l.allowCreate {
		// Try to wipe the pid if we have write perm
		_ = f.Truncate(0)
	}

	err := flock.Funlock(f)
	if err != nil {
		return err
	}

	l.f = nil

	return nil
}

func (l *Flock) TryRLock(ctx context.Context) (bool, error) {
	return l.tryLock(ctx, true, func(f *os.File, ro bool) (bool, error) {
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
	err := l.fs.RemoveAll(l.path)
	if err != nil {
		return err
	}

	return nil
}
