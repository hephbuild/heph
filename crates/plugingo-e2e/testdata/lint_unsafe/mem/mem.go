// Package mem imports `unsafe`, a pseudo-package with no archive: unitchecker
// still resolves it through the cfg's ImportMap, so linting it exercises that the
// driver carries the entry.
package mem

import "unsafe"

// SizeOfInt64 reports the size of an int64.
func SizeOfInt64() uintptr {
	var v int64
	return unsafe.Sizeof(v)
}
