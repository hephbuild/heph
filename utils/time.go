package utils

import (
	"strconv"
	"time"
)

var divs = []time.Duration{
	time.Duration(1), time.Duration(10), time.Duration(100), time.Duration(1000)}

func RoundDuration(d time.Duration, digits int) time.Duration {
	switch {
	case d > time.Second:
		d = d.Round(time.Second / divs[digits])
	case d > time.Millisecond:
		d = d.Round(time.Millisecond / divs[digits])
	case d > time.Microsecond:
		d = d.Round(time.Microsecond / divs[digits])
	}
	return d
}

func FormatDuration(d time.Duration) string {
	d = RoundDuration(d, 1)

	if d < 100*time.Millisecond {
		return RoundDuration(d, 0).String()
	}

	if d < time.Second {
		return strconv.FormatFloat(d.Seconds(), 'f', 1, 64) + "s"
	}

	if d < time.Minute {
		if d%time.Second == 0 {
			return strconv.FormatInt(int64(d/time.Second), 10) + ".0s"
		}

		return RoundDuration(d, 1).String()
	}

	return RoundDuration(d, 0).String()
}
