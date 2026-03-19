package htime

import (
	"math/rand/v2"
	"time"
)

// JitterPercent returns d +/- a random jitter up to percent% of d.
// percent should be in the range [0, 1], e.g. 0.1 for 10%.
func JitterPercent(d time.Duration, percent float64) time.Duration {
	return JitterAbs(d, time.Duration(float64(d)*percent))
}

// JitterAbs returns d +/- a random jitter up to abs in magnitude.
func JitterAbs(d time.Duration, abs time.Duration) time.Duration {
	jitter := time.Duration(float64(abs) * (rand.Float64()*2 - 1)) //nolint:gosec

	return d + jitter
}
