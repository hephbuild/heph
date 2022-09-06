package utils

import (
	"fmt"
	"github.com/vmihailenco/msgpack/v5"
	"go.starlark.net/starlark"
	"reflect"
	"sort"
)

func FromStarlark(v starlark.Value) interface{} {
	switch v := v.(type) {
	case starlark.NoneType:
		return nil
	case starlark.String:
		return string(v)
	case starlark.Bool:
		return bool(v)
	case *starlark.Dict:
		data := map[interface{}]interface{}{}

		for _, e := range v.Items() {
			data[FromStarlark(e.Index(0))] = FromStarlark(e.Index(1))
		}

		return data
	case *starlark.List:
		data := []interface{}{}

		it := v.Iterate()
		var value starlark.Value
		for it.Next(&value) {
			value := value
			data = append(data, value)
		}

		return data
	default:
		panic(fmt.Sprintf("FromStarlark: unhandled type %T", v))
	}

	return nil
}

func FromGo(v interface{}) starlark.Value {
	switch v := v.(type) {
	case string:
		return starlark.String(v)
	case bool:
		return starlark.Bool(v)
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

	return starlark.None
}

func HashStarlark(h Hash, value starlark.Value) {
	if value == nil {
		h.Write([]byte{0})
		return
	}

	switch value := value.(type) {
	case *starlark.Dict:
		rkeys := value.Keys()
		keym := make(map[string]starlark.Value, len(rkeys))
		keys := make([]string, 0, len(rkeys))

		for _, k := range rkeys {
			s := k.String()
			keym[s] = k
		}

		sort.Strings(keys)

		for _, ks := range keys {
			k := keym[ks]
			HashStarlark(h, k)
			v, _, _ := value.Get(k)
			HashStarlark(h, v)
		}
		return
	case *starlark.List:
		it := value.Iterate()
		var v starlark.Value
		for it.Next(&v) {
			HashStarlark(h, v)
		}
		return
	}

	u, err := value.Hash()
	if err == nil {
		h.UI32(u)
		return
	}

	enc := msgpack.NewEncoder(h)
	err = enc.Encode(FromStarlark(value))
	if err != nil {
		panic(err)
	}
}
