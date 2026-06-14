// pkgb has an internal _test.go that imports pkga, which itself imports pkgb.
// This creates the cycle that triggers Go's linker fingerprint check.
package pkgb

func Value() int {
	return 42
}
