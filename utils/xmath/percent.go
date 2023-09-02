package xmath

import (
	"fmt"
	"golang.org/x/exp/constraints"
	"strings"
)

type Number interface {
	constraints.Integer | constraints.Float
}

func Ratio[A, B Number](num A, deno B) float64 {
	return float64(num) / float64(deno)
}

func Percent[A, B Number](num A, deno B) float64 {
	return Ratio(num, deno) * 100.0
}

func FormatPercent[T Number](s string, percent T) string {
	if percent < 0 || percent > 100 {
		parts := strings.Split(s, "[P]")
		for i, part := range parts {
			parts[i] = strings.TrimSpace(part)
		}

		return strings.Join(parts, "")
	} else {
		return strings.ReplaceAll(s, "[P]", fmt.Sprintf("%v%%", percent))
	}
}
