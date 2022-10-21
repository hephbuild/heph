package main

import (
	"fmt"
	"strconv"
	"strings"
)

func genArray(es []string, l int, nl bool) string {
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
		sb.WriteString(e)
		sb.WriteString(",")
		if nl {
			sb.WriteString("\n")
		}
	}
	for i := 0; i < l-1; i++ {
		sb.WriteString("    ")
	}
	sb.WriteString("]")
	return sb.String()
}

func genStringArray(es []string, l int) string {
	qes := make([]string, 0, len(es))
	for _, e := range es {
		qes = append(qes, strconv.Quote(e))
	}

	return genArray(qes, l, true)
}

func genStringArrayInline(es []string, l int) string {
	qes := make([]string, 0, len(es))
	for _, e := range es {
		qes = append(qes, strconv.Quote(e))
	}

	return genArray(qes, l, false)
}

func joinedArrays(es []string) string {
	if len(es) == 0 {
		return "[]"
	}

	return strings.Join(es, "+")
}

func genVariant(v PkgCfgVariant, onlyOsArch bool) string {
	if v.OS == "" || v.ARCH == "" {
		panic("empty os/arch")
	}

	s := fmt.Sprintf("os=%v, arch=%v", strconv.Quote(v.OS), strconv.Quote(v.ARCH))

	if !onlyOsArch && len(v.Tags) > 0 {
		s += fmt.Sprintf(", tags=%v", genStringArrayInline(v.Tags, 0))
	}

	return s
}
