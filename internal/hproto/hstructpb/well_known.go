package hstructpb

import (
	"fmt"
	"reflect"
)

type SpecStrings []string

func (s *SpecStrings) MapstructureDecode(v any) error {
	if v == nil {
		*s = nil

		return nil
	}

	rv := reflect.ValueOf(v)

	switch rv.Kind() {
	case reflect.String:
		*s = []string{rv.Interface().(string)} //nolint:errcheck
		return nil
	case reflect.Slice, reflect.Array:
		values := make([]string, 0, rv.Len())
		for i := range rv.Len() {
			elem := rv.Index(i)

			if v, ok := elem.Interface().(string); ok {
				values = append(values, v)
			} else {
				return fmt.Errorf("expected string, got %v", elem.Type())
			}
		}
		*s = values
		return nil
	default:
		return fmt.Errorf("expected string or []string, got %q", toType(v))
	}
}
