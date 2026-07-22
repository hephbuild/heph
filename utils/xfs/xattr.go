package xfs

import (
	"errors"
	"syscall"

	"github.com/pkg/xattr"
)

// CodegenXattr is the extended-attribute name stamped on net-new codegen files
// written back to the tree (value = the generator's addr). Raw source reads must
// skip these so a generated file is never double-sourced.
const CodegenXattr = "user.heph.codegen"

// V0Xattr marks a file as written by the v0 tree layout. Value is always "1".
const V0Xattr = "user.heph.v0"

// SetXattr writes an extended attribute on path. Filesystems that do not support
// xattrs (or any not-supported error) are treated as a no-op so trees on such
// filesystems never break; other errors are returned.
func SetXattr(path, name string, value []byte) error {
	err := xattr.LSet(path, name, value)
	if err != nil && isXattrUnsupported(err) {
		return nil
	}
	return err
}

// HasXattr reports whether path carries the extended attribute name. A
// filesystem that does not support xattrs (or any error) is treated as
// "not present" so globbing/file reads never break on such trees.
func HasXattr(path, name string) bool {
	_, err := xattr.LGet(path, name)
	return err == nil
}

func isXattrUnsupported(err error) bool {
	if errors.Is(err, syscall.ENOTSUP) || errors.Is(err, syscall.EOPNOTSUPP) {
		return true
	}
	var xerr *xattr.Error
	if errors.As(err, &xerr) {
		return errors.Is(xerr.Err, syscall.ENOTSUP) || errors.Is(xerr.Err, syscall.EOPNOTSUPP)
	}
	return false
}
