package main

import (
	"fmt"
	"reflect"
	"sort"
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

func genDict(d map[string]interface{}, l int, nl bool) string {
	if len(d) == 0 {
		return "{}"
	}

	return genArgValue1(d, l, nl)
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

func genVariant(v PkgCfgVariant, tags, linkflags, linkdeps bool) string {
	if v.OS == "" || v.ARCH == "" {
		panic("empty os/arch")
	}

	s := fmt.Sprintf("os=%v, arch=%v", strconv.Quote(v.OS), strconv.Quote(v.ARCH))

	if tags && len(v.Tags) > 0 {
		s += ",\n    tags=" + genStringArrayInline(v.Tags, 0)
	}

	if linkflags && v.Link.Flags != "" {
		s += ",\n    flags=" + strconv.Quote(v.Link.Flags)
	}

	if linkdeps {
		if len(v.Link.Deps) > 0 {
			s += ",\n    deps=" + genDict(v.Link.Deps, 1, true)
		}

		if len(v.Link.HashDeps) > 0 {
			s += ",\n    hash_deps=" + genDict(v.Link.HashDeps, 1, true)
		}
	}

	return s
}

func genArgValue(v interface{}, withNl bool) string {
	return genArgValue1(v, 0, withNl)
}

func genArgValue1(v interface{}, p int, withNl bool) string {
	var nl string
	if withNl {
		nl = "\n"
	}

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

			d := map[string]interface{}{}

			it := rv.MapRange()
			for it.Next() {
				k := it.Key().Interface().(string)
				v := it.Value().Interface()

				d[k] = v
			}

			ks := make([]string, 0, len(d))
			for k := range d {
				ks = append(ks, k)
			}
			sort.Strings(ks)

			var sb strings.Builder
			sb.WriteString("{\n")
			for _, k := range ks {
				e := d[k]

				for i := 0; i < p+1; i++ {
					sb.WriteString("    ")
				}
				sb.WriteString(genArgValue(k, false))
				sb.WriteString(": ")
				sb.WriteString(genArgValue(e, false))
				sb.WriteString(",")
				if withNl {
					sb.WriteString("\n")
				}
			}
			for i := 0; i < p; i++ {
				sb.WriteString("    ")
			}
			sb.WriteString("}")
			return sb.String()
		case reflect.Slice, reflect.Array:
			vs := make([]string, 0)
			for i := 0; i < rv.Len(); i++ {
				vs = append(vs, genArgValue(rv.Index(i).Interface(), false))
			}

			return "[" + nl + strings.Join(vs, ","+nl) + nl + "]"
		}

		panic(fmt.Sprintf("unhandled %T", v))
	}
}

func genArgs(m map[string]interface{}) string {
	kvs := make([]string, 0)

	for k, v := range m {
		kvs = append(kvs, k+"="+genArgValue(v, true))
	}

	return strings.Join(kvs, ",\n")
}
