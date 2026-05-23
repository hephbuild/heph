// Internal _test.go cannot import pkga because pkga imports pkgb (cycle
// rejected by Go for internal tests). Internal test stays self-contained.
package pkgb

import "testing"

func TestValue(t *testing.T) {
	if Value() != 42 {
		t.Fatal("expected 42")
	}
}
