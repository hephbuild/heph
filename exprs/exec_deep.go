package exprs

import (
	"fmt"
	"reflect"
)

func ExecDeep(obj any, funcs map[string]Func) error {
	v := reflect.ValueOf(obj)
	if v.Kind() != reflect.Ptr {
		panic(fmt.Sprintf("expected pointer, got %T", obj))
	}

	v = v.Elem()

	nv, err := execReflect(v, funcs)
	if err != nil {
		return err
	}

	v.Set(nv)

	return nil
}

func execReflect(v reflect.Value, funcs map[string]Func) (reflect.Value, error) {
	switch v.Kind() {
	case reflect.String:
		v2, err := Exec(v.Interface().(string), funcs)
		if err != nil {
			return reflect.Value{}, err
		}

		return reflect.ValueOf(v2), nil
	case reflect.Ptr:
		if v.IsNil() {
			return v, nil
		}

		ev, err := execReflect(v.Elem(), funcs)
		if err != nil {
			return reflect.Value{}, err
		}

		p := reflect.New(v.Type().Elem())
		p.Elem().Set(ev)

		return p, nil
	case reflect.Interface:
		if v.IsNil() {
			return v, nil
		}

		return execReflect(v.Elem(), funcs)
	case reflect.Map:
		return execReflectMap(v, funcs)
	case reflect.Struct:
		return execReflectStruct(v, funcs)
	case reflect.Slice, reflect.Array:
		for i := 0; i < v.Len(); i++ {
			e := v.Index(i)

			ev, err := execReflect(e, funcs)
			if err != nil {
				return reflect.Value{}, err
			}

			e.Set(ev)
		}

		return v, nil
	}

	return v, nil
}

func execReflectMap(obj reflect.Value, funcs map[string]Func) (reflect.Value, error) {
	if obj.IsNil() {
		return obj, nil
	}

	m := reflect.MakeMapWithSize(reflect.MapOf(obj.Type().Key(), obj.Type().Elem()), obj.Len())

	it := obj.MapRange()
	for it.Next() {
		kv, err := execReflect(it.Key(), funcs)
		if err != nil {
			return reflect.Value{}, err
		}

		vv, err := execReflect(it.Value(), funcs)
		if err != nil {
			return reflect.Value{}, err
		}

		m.SetMapIndex(kv, vv)
	}

	return m, nil
}

func execReflectStruct(obj reflect.Value, funcs map[string]Func) (reflect.Value, error) {
	s := reflect.New(obj.Type()).Elem()

	for i := 0; i < s.NumField(); i++ {
		f := s.Field(i)

		if !f.CanAddr() {
			continue
		}

		f.Set(obj.Field(i))

		fv, err := execReflect(f, funcs)
		if err != nil {
			return reflect.Value{}, err
		}

		f.Set(fv)
	}

	return s, nil
}
