package xmath

import (
	"fmt"
	"golang.org/x/exp/constraints"
	"strings"
)

func Percent[A, B constraints.Integer | constraints.Float](num A, deno B) float64 {
	return (float64(num) / float64(deno)) * 100.0
}

func FormatPercent[T constraints.Integer | constraints.Float](s string, percent T) string {
	if percent > 100 {
		parts := strings.Split(s, "%P")
		for i, part := range parts {
			parts[i] = strings.TrimSpace(part)
		}

		return strings.Join(parts, "")
	} else {
		return strings.ReplaceAll(s, "%P", fmt.Sprintf("%v%%", percent))
	}
}
