package hlocks

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/internal/flock"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hfs"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"sync"
	"syscall"
	"time"
)

func NewFlock2(fs hfs.OS, name, path string, allowCreate bool) *Flock {
	if name == "" {
		name = fs.Path(path)
	}
	l := &Flock{fs: fs, path: path, name: name, allowCreate: allowCreate}

	return l
}

func NewFlock(fs hfs.OS, name, path string) *Flock {
	return NewFlock2(fs, name, path, true)
}

type Flock struct {
	name        string
	rc          int          // count of read locks, allows multiple rlock on this instance
	opm         sync.RWMutex // op mutex, protects this instances against races with with itself
	lm          sync.RWMutex // lock mutex, protects this instances specifically against lock races
	path        string
	f           *os.File
	cleanup     runtime.Cleanup
	fs          hfs.OS
	allowCreate bool
}

func (l *Flock) tryLock(ctx context.Context, ro bool, onErr func(f *os.File, ro bool) (bool, error)) (bool, error) {
	l.lm.Lock()
	defer l.lm.Unlock()

	fhow := syscall.O_RDWR
	if ro {
		fhow = syscall.O_RDONLY

		l.opm.Lock()
		if l.rc > 0 {
			l.rc++
			l.opm.Unlock()

			return true, nil
		}
		l.opm.Unlock()
	}

	if l.allowCreate {
		err := l.fs.MkdirAll(filepath.Dir(l.path), hfs.ModePerm)
		if err != nil {
			return false, err
		}

		fhow |= os.O_CREATE
	}

	hf, err := l.fs.Open(l.path, fhow, 0644)
	if err != nil {
		return false, err
	}

	f := hf.(*os.File) //nolint:errcheck

	err = flock.Flock(f, ro, false)
	if err != nil { //nolint:nestif
		if flock.IsErrWouldBlock(err) {
			ok, err := onErr(f, ro)
			if err != nil {
				_ = f.Close()

				return false, fmt.Errorf("acquire lock for %s: %w", l.name, err)
			}

			if !ok {
				_ = f.Close()

				return false, nil
			}
		} else {
			_ = f.Close()

			return false, err
		}
	}

	l.opm.Lock()
	defer l.opm.Unlock()

	if l.rc == 0 {
		var stack string
		if true {
			buf := make([]byte, 4096)
			n := runtime.Stack(buf, false)

			stack = "\n" + string(buf[:n])
		}

		l.cleanup.Stop()
		l.cleanup = runtime.AddCleanup(l, func(f *os.File) {
			panic(fmt.Sprintf("Flock file is being freed, but lock is stil held: %v%v", f.Name(), stack))
		}, f)
		l.f = f
	}

	if ro {
		l.rc++
	}

	if !ro && l.allowCreate {
		if err := f.Truncate(0); err == nil {
			_, _ = f.WriteAt([]byte(strconv.Itoa(os.Getpid())), 0)
		}
	}

	return true, nil
}

func (l *Flock) lock(ctx context.Context, ro bool) error {
	_, err := l.tryLock(ctx, ro, func(f *os.File, ro bool) (bool, error) {
		doneCh := make(chan struct{})
		logDoneCh := make(chan struct{})

		go func() {
			defer close(logDoneCh)

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
					return
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
						hlog.From(ctx).Debug(fmt.Sprintf("Another routine locked %v, waiting...", l.name))
					} else {
						processDetails := getProcessDetails(pid)

						hlog.From(ctx).Debug(fmt.Sprintf("Process %v locked %v, waiting...", processDetails, l.name))
					}

					return
				} else {
					hlog.From(ctx).Debug(fmt.Sprintf("Another process locked %v, waiting...", l.name))

					select {
					case <-doneCh:
						return
					case <-time.After(time.Second):
					}
				}
			}
		}()

		defer func() {
			// wait for goroutine to go away
			<-logDoneCh
		}()
		defer close(doneCh)

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
	return l.unlock(false)
}

func (l *Flock) unlock(ro bool) error {
	l.opm.Lock()
	defer l.opm.Unlock()

	f := l.f

	if f == nil {
		return fmt.Errorf("attempting to release lock when not held ")
	}

	if l.allowCreate {
		// Try to wipe the pid if we have write perm
		_ = f.Truncate(0)
	}

	if ro {
		l.rc--

		if l.rc > 0 { // some other goroutine holds an RLock on this instance, dont release the file yet
			return nil
		}
	}

	err := flock.Funlock(f)
	if err != nil {
		return err
	}

	l.cleanup.Stop()
	l.f = nil
	l.rc = 0

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
	return l.unlock(true)
}

func (l *Flock) Clean() error {
	err := l.fs.RemoveAll(l.path)
	if err != nil {
		return err
	}

	return nil
}
