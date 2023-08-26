package utils

import (
	"fmt"
	"go.starlark.net/starlark"
	"reflect"
)

func FromStarlark(v starlark.Value) interface{} {
	switch v := v.(type) {
	case starlark.NoneType:
		return nil
	case starlark.String:
		return string(v)
	case starlark.Bool:
		return bool(v)
	case starlark.Int:
		vi, _ := v.Int64()
		return vi
	case starlark.Float:
		return float64(v)
	case *starlark.Dict:
		data := make(map[interface{}]interface{}, v.Len())

		for _, e := range v.Items() {
			data[FromStarlark(e.Index(0))] = FromStarlark(e.Index(1))
		}

		return data
	case *starlark.List:
		data := make([]interface{}, 0, v.Len())

		it := v.Iterate()
		defer it.Done()

		var value starlark.Value
		for it.Next(&value) {
			data = append(data, FromStarlark(value))
		}

		return data
	default:
		panic(fmt.Sprintf("FromStarlark: unhandled type %T", v))
	}
}

func FromGo(v interface{}) starlark.Value {
	if v == nil {
		return starlark.None
	}

	switch v := v.(type) {
	case string:
		return starlark.String(v)
	case bool:
		return starlark.Bool(v)
	case int:
		return starlark.MakeInt(v)
	case int8:
		return starlark.MakeInt(int(v))
	case int16:
		return starlark.MakeInt(int(v))
	case int32:
		return starlark.MakeInt(int(v))
	case int64:
		return starlark.MakeInt64(v)
	case float32:
		return starlark.Float(v)
	case float64:
		return starlark.Float(v)
	default:
		rv := reflect.ValueOf(v)
		switch rv.Kind() {
		case reflect.Map:
			dict := &starlark.Dict{}

			it := rv.MapRange()
			for it.Next() {
				k := it.Key().Interface()
				v := it.Value().Interface()

				err := dict.SetKey(FromGo(k), FromGo(v))
				if err != nil {
					panic(err)
				}
			}

			return dict
		case reflect.Slice, reflect.Array:
			list := &starlark.List{}
			for i := 0; i < rv.Len(); i++ {
				err := list.Append(FromGo(rv.Index(i).Interface()))
				if err != nil {
					panic(err)
				}
			}

			return list
		}

		panic(fmt.Sprintf("FromGo: unhandled type %T", v))
	}
}
