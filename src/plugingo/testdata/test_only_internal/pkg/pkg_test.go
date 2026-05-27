package pkg

import "testing"

func TestBasic(t *testing.T) {
	if 1+1 != 2 {
		t.Fatal("math is broken")
	}
}
