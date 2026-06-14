package pkga

import "example.com/with_test_cycle/pkgb"

func Wrap(v int) int {
	return v * 2
}

// Used to ensure pkga genuinely imports pkgb (cycle through tests).
var _ = pkgb.Value
