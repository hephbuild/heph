package pkg_test

import (
	"testing"

	"example.com/testxtestonly/pkg"
)

func TestExternal(t *testing.T) {
	if pkg.Helper != 42 {
		t.Fatal("helper missing")
	}
}
