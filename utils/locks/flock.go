package locks

import (
	"context"
	"fmt"
	log "github.com/hephbuild/heph/log/liblog"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/utils/flock"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/worker2"
	"os"
	"strconv"
	"sync"
	"syscall"
	"time"
)

func NewFlock(name, p string) *Flock {
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

func (l *Flock) tryLock(ctx context.Context, ro bool, onErr func(f *os.File, ro bool) (bool, error)) (bool, error) {
	logger := log.FromContext(ctx)

	fhow := syscall.O_RDWR
	if ro {
		fhow = syscall.O_RDONLY

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
			_ = f.Close()
		}
	}()

	logger.Debugf("Attempting to acquire lock for %s...", f.Name())
	err = flock.Flock(f, ro, false)
	if err != nil {
		if flock.IsErrWouldBlock(err) {
			ok, err := onErr(f, ro)
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

				pidb, _ := os.ReadFile(f.Name())
				pidStr := string(pidb)

				if len(pidStr) > 0 {
					pid, err := strconv.Atoi(pidStr)
					if err != nil {
						pid = -1
					}

					if os.Getpid() == pid {
						status.Emit(ctx, status.String(fmt.Sprintf("Another job locked %v, waiting...", l.name)))
					} else {
						processDetails := getProcessDetails(pid)

						status.Emit(ctx, status.String(fmt.Sprintf("Process %v locked %v, waiting...", processDetails, l.name)))
					}

					break
				} else {
					status.Emit(ctx, status.String(fmt.Sprintf("Another process locked %v, waiting...", l.name)))
					time.Sleep(time.Second)
				}
			}
		}()

		lockCh := make(chan error, 1)

		go func() {
			defer close(lockCh)
			lockCh <- flock.Flock(f, ro, true)
		}()

		err := worker2.WaitChanE(ctx, lockCh)
		if err != nil {
			return false, err
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

	// Try to wipe the pid if we have write perm
	_ = f.Truncate(0)

	err := flock.Flunlock(f)
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
	err := os.RemoveAll(l.path)
	if err != nil {
		return err
	}

	return nil
}
