//go:build linux

package lwl

import (
	"fmt"
	"golang.org/x/sys/unix"
	log "heph/hlog"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"unsafe"
)

func MemfdCreate(path string) (r1 uintptr, err error) {
	s, err := syscall.BytePtrFromString(path)
	if err != nil {
		return 0, err
	}

	r1, _, errno := syscall.Syscall(319, uintptr(unsafe.Pointer(s)), 0, 0)

	if int(r1) == -1 {
		return r1, errno
	}

	return r1, nil
}

func CopyToMem(fd uintptr, buf []byte) (err error) {
	_, err = syscall.Write(int(fd), buf)
	if err != nil {
		return err
	}

	return nil
}

func Execveat(fd uintptr, pathname string, argv []string, envv []string, flags int) error {
	pathnamep, err := syscall.BytePtrFromString(pathname)
	if err != nil {
		return err
	}

	argvp, err := syscall.SlicePtrFromStrings(argv)
	if err != nil {
		return err
	}

	envvp, err := syscall.SlicePtrFromStrings(envv)
	if err != nil {
		return err
	}

	_, _, errno := syscall.Syscall6(
		unix.SYS_EXECVEAT,
		fd,
		uintptr(unsafe.Pointer(pathnamep)),
		uintptr(unsafe.Pointer(&argvp[0])),
		uintptr(unsafe.Pointer(&envvp[0])),
		uintptr(flags),
		0,
	)
	if errno != 0 {
		return fmt.Errorf("SYS_EXECVEAT: %w", errno)
	}

	return nil
}

func Execute(path string, args []string) error {
	lwbin, err := Read(path)
	if err != nil {
		return fmt.Errorf("Read: %w", err)
	}

	libDir := filepath.Join(os.TempDir(), "lwb-lib-"+strings.ReplaceAll(path, "/", "_"))
	log.Tracef("Libdir %v", libDir)

	binPath, err := lwbin.Unpack(libDir)
	if err != nil {
		return fmt.Errorf("Unpack: %w", err)
	}

	var ldpath string
	for _, lib := range lwbin.Libs {
		base := filepath.Base(lib.Path)
		if strings.Contains(base, "ld") {
			ldpath = filepath.Join(libDir, base)
		}
	}

	if ldpath == "" {
		return fmt.Errorf("cannot find ld in libs")
	}

	env := os.Environ()
	env = append(env, "LD_LIBRARY_PATH="+libDir)

	exargs := append([]string{ldpath, binPath}, args...)

	err = syscall.Exec(ldpath, exargs, env)
	if err != nil {
		return fmt.Errorf("Exec: %v, %w", binPath, err)
	}

	return nil

	fd, err := MemfdCreate("/file.bin")
	if err != nil {
		return fmt.Errorf("MemfdCreate: %w", err)
	}

	err = CopyToMem(fd, lwbin.Bin)
	if err != nil {
		return fmt.Errorf("CopyToMem: %w", err)
	}

	err = Execveat(fd, "", args, nil, unix.AT_EMPTY_PATH)
	if err != nil {
		return fmt.Errorf("Execveat: %w", err)
	}

	return nil
}
