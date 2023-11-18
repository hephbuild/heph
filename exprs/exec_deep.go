package exprs

import (
	"context"
	"fmt"
	"reflect"
)

func ExecDeep(ctx context.Context, obj any, funcs map[string]Func) error {
	v := reflect.ValueOf(obj)
	if v.Kind() != reflect.Ptr {
		panic(fmt.Sprintf("expected pointer, got %T", ctx))
	}

	v = v.Elem()

	nv, err := execReflect(ctx, v, funcs)
	if err != nil {
		return err
	}

	v.Set(nv)

	return nil
}

func execReflect(ctx context.Context, v reflect.Value, funcs map[string]Func) (reflect.Value, error) {
	if err := ctx.Err(); err != nil {
		return reflect.Value{}, err
	}

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

		ev, err := execReflect(ctx, v.Elem(), funcs)
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

		return execReflect(ctx, v.Elem(), funcs)
	case reflect.Map:
		return execReflectMap(ctx, v, funcs)
	case reflect.Struct:
		return execReflectStruct(ctx, v, funcs)
	case reflect.Slice, reflect.Array:
		if v.IsNil() {
			return v, nil
		}

		s := reflect.MakeSlice(reflect.SliceOf(v.Type().Elem()), 0, v.Len())

		for i := 0; i < v.Len(); i++ {
			e := v.Index(i)

			ev, err := execReflect(ctx, e, funcs)
			if err != nil {
				return reflect.Value{}, err
			}

			s = reflect.Append(s, ev)
		}

		return s, nil
	}

	return v, nil
}

func execReflectMap(ctx context.Context, obj reflect.Value, funcs map[string]Func) (reflect.Value, error) {
	if obj.IsNil() {
		return obj, nil
	}

	m := reflect.MakeMapWithSize(reflect.MapOf(obj.Type().Key(), obj.Type().Elem()), obj.Len())

	it := obj.MapRange()
	for it.Next() {
		kv, err := execReflect(ctx, it.Key(), funcs)
		if err != nil {
			return reflect.Value{}, err
		}

		vv, err := execReflect(ctx, it.Value(), funcs)
		if err != nil {
			return reflect.Value{}, err
		}

		m.SetMapIndex(kv, vv)
	}

	return m, nil
}

func execReflectStruct(ctx context.Context, obj reflect.Value, funcs map[string]Func) (reflect.Value, error) {
	s := reflect.New(obj.Type()).Elem()

	for i := 0; i < s.NumField(); i++ {
		f := s.Field(i)

		if !f.CanAddr() {
			continue
		}

		f.Set(obj.Field(i))

		fv, err := execReflect(ctx, f, funcs)
		if err != nil {
			return reflect.Value{}, err
		}

		f.Set(fv)
	}

	return s, nil
}
