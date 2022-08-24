package main

import (
	"strconv"
	"strings"
)

func genArrayNone(es []string, l int) string {
	if es == nil {
		return "None"
	}

	return genArray(es, l)
}

func genArray(es []string, l int) string {
	if len(es) == 0 {
		return "[]"
	}

	m := map[string]struct{}{}

	var sb strings.Builder
	sb.WriteString("[\n")
	for _, e := range es {
		if _, ok := m[e]; ok {
			continue
		}
		m[e] = struct{}{}

		for i := 0; i < l; i++ {
			sb.WriteString("    ")
		}
		sb.WriteString(strconv.Quote(e))
		sb.WriteString(",\n")
	}
	for i := 0; i < l-1; i++ {
		sb.WriteString("    ")
	}
	sb.WriteString("]")
	return sb.String()
}
