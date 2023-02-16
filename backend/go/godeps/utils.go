package main

import (
	"fmt"
	"reflect"
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

func genVariant(v PkgCfgVariant, tags, ldflags bool) string {
	if v.OS == "" || v.ARCH == "" {
		panic("empty os/arch")
	}

	s := fmt.Sprintf("os=%v, arch=%v", strconv.Quote(v.OS), strconv.Quote(v.ARCH))

	if tags && len(v.Tags) > 0 {
		s += ", tags=" + genStringArrayInline(v.Tags, 0)
	}

	if ldflags && v.LDFlags != "" {
		s += ", ldflags=" + strconv.Quote(v.LDFlags)
	}

	return s
}

func genArgValue(v interface{}, nl string) string {
	switch v := v.(type) {
	case string:
		return strconv.Quote(v)
	case bool:
		if v {
			return "True"
		} else {
			return "False"
		}
	case int, int8, int16, int32, int64, uint, uint8, uint16, uint32, uint64, float32, float64:
		return fmt.Sprint(v)
	default:
		rv := reflect.ValueOf(v)
		switch rv.Kind() {
		case reflect.Map:
			es := make([]string, 0)
			it := rv.MapRange()
			for it.Next() {
				k := it.Key().Interface()
				v := it.Value().Interface()

				es = append(es, genArgValue(k, "")+":"+genArgValue(v, ""))
			}

			return "{" + nl + strings.Join(es, ","+nl) + nl + "}"
		case reflect.Slice, reflect.Array:
			vs := make([]string, 0)
			for i := 0; i < rv.Len(); i++ {
				vs = append(vs, genArgValue(rv.Index(i).Interface(), ""))
			}

			return "[" + nl + strings.Join(vs, ","+nl) + nl + "]"
		}

		panic(fmt.Sprintf("unhandled %T", v))
	}
}

func genArgs(m map[string]interface{}) string {
	kvs := make([]string, 0)

	for k, v := range m {
		kvs = append(kvs, k+"="+genArgValue(v, "\n"))
	}

	return strings.Join(kvs, ",\n")
}
