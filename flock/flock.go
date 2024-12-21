package flock

import (
	"errors"
	"fmt"
	"golang.org/x/sys/unix"
	"os"
	"syscall"
)

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

func Flock(f *os.File, shared, blocking bool) error {
	lhow := syscall.LOCK_EX
	if shared {
		lhow = syscall.LOCK_SH
	}

	if !blocking {
		lhow |= syscall.LOCK_NB
	}

	err := fileDoFd(f, func(fd uintptr) error {
		return syscall.Flock(int(fd), lhow)
	})
	if err != nil {
		return err
	}

	return err
}

func Funlock(f *os.File) error {
	err := fileDoFd(f, func(fd uintptr) error {
		return syscall.Flock(int(fd), syscall.LOCK_UN)
	})
	if err != nil {
		return fmt.Errorf("release lock for %s: %s", f.Name(), err)
	}

	err = f.Close()
	if err != nil {
		return fmt.Errorf("close lock file %s: %s", f.Name(), err)
	}

	return nil
}

func IsErrWouldBlock(err error) bool {
	var errno unix.Errno
	if ok := errors.As(err, &errno); ok && errno == unix.EWOULDBLOCK {
		return true
	}

	return false
}
