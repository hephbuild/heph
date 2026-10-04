package lib_test

import (
	"testing"

	"example.com/xtest/lib"
)

func TestDouble(t *testing.T) {
	if got := lib.Double(2); got != 4 {
		t.Fatalf("Double(2) = %d, want 4", got)
	}
}
