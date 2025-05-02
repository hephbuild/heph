package hstructpb

import (
	"fmt"
	"reflect"

	"github.com/go-viper/mapstructure/v2"
	"google.golang.org/protobuf/types/known/structpb"
)

type MapstructureDecoder interface {
	MapstructureDecode(v any) error
}

func Decode[T any](from any) (T, error) {
	var to T
	err := DecodeTo(from, &to)
	return to, err
}

func DecodeTo(from, to any) error {
	dec, err := mapstructure.NewDecoder(&mapstructure.DecoderConfig{
		Result: to,
		DecodeHook: mapstructure.ComposeDecodeHookFunc(
			mapstructure.DecodeHookFuncKind(func(_ reflect.Kind, _ reflect.Kind, v any) (any, error) {
				if v, ok := v.(*structpb.Value); ok {
					return v.AsInterface(), nil
				}

				return v, nil
			}),
			mapstructure.DecodeHookFuncValue(func(from reflect.Value, to reflect.Value) (any, error) {
				if i, ok := to.Addr().Interface().(MapstructureDecoder); ok {
					err := i.MapstructureDecode(from.Interface())

					return i, err
				}

				return from.Interface(), nil
			}),
		),
		ErrorUnused: true,
	})
	if err != nil {
		return err
	}

	err = dec.Decode(from)
	if err != nil {
		return err
	}

	return nil
}

func DecodeSlice[T any](v any) ([]T, error) {
	var zero T

	if v, err := Decode[[]T](v); err == nil {
		return v, nil
	}

	if vas, err := Decode[[]any](v); err == nil {
		out := make([]T, 0, len(vas))

		for i, va := range vas {
			v, err := Decode[T](va)
			if err != nil {
				return nil, fmt.Errorf("%v: expected %T, got %T", i, zero, va)
			}

			out = append(out, v)
		}

		return out, nil
	}

	return nil, fmt.Errorf("expected %T or []%T, got %T", zero, zero, v)
}
