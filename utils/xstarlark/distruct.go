package xstarlark

import (
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"go.starlark.net/starlark"
	"go.starlark.net/starlarkstruct"
	"strings"
)

type KV interface {
	Items() StringItems
}

type StringItem struct {
	Key   string
	Value starlark.Value
}

type StringItems []StringItem

func (s StringItems) Tuples() []starlark.Tuple {
	return ads.Map(s, func(t StringItem) starlark.Tuple {
		return starlark.Tuple{starlark.String(t.Key), t.Value}
	})
}

type structkv struct {
	s     *starlarkstruct.Struct
	items StringItems
}

func (s *structkv) Items() StringItems {
	if s.items == nil {
		s.items = ads.Map(s.s.AttrNames(), func(name string) StringItem {
			v, err := s.s.Attr(name)
			if err != nil {
				panic(err)
			}
			return StringItem{name, v}
		})
	}

	return s.items
}

type dictkv struct {
	dict  *starlark.Dict
	items StringItems
}

func (s *dictkv) Items() StringItems {
	if s.items == nil {
		s.items = ads.Map(s.dict.Items(), func(item starlark.Tuple) StringItem {
			k := item[0]
			v := item[1]

			return StringItem{k.(starlark.String).GoString(), v}
		})
	}

	return s.items
}

func kvFrom(v starlark.Value) (KV, error) {
	switch v := v.(type) {
	case *starlark.Dict:
		kv := &dictkv{}
		kv.dict = v
		return kv, nil
	case *starlarkstruct.Struct:
		kv := &structkv{}
		kv.s = v
		return kv, nil
	default:
		return nil, fmt.Errorf("expected struct or dict, got %v", v.Type())
	}
}

func UnpackDistruct(v starlark.Value) (*Distruct, error) {
	var d Distruct
	err := d.Unpack(v)
	if err != nil {
		return nil, err
	}

	return &d, nil
}

// Distruct is a Type that can unpack a struct or dist
type Distruct struct {
	items []StringItem
}

func (s *Distruct) Items() StringItems {
	return s.items
}

func (s *Distruct) Unpack(v starlark.Value) error {
	kv, err := kvFrom(v)
	if err != nil {
		return err
	}

	s.items = kv.Items()
	return nil
}

func keysFromPairs(pairs ...interface{}) []string {
	keys := make([]string, 0, len(pairs)/2)

	for i, v := range pairs {
		if i%2 != 0 {
			continue
		}

		s, ok := v.(string)
		if !ok {
			continue
		}

		s = strings.TrimSuffix(s, "?")

		keys = append(keys, s)
	}

	return keys
}

func UnpackDistructTo(v starlark.Value, pairs ...interface{}) error {
	d, err := UnpackDistruct(v)
	if err != nil {
		return fmt.Errorf("expected struct or dict with keys %v, got %v", keysFromPairs(pairs), v.Type())
	}

	return starlark.UnpackArgs("", nil, d.Items().Tuples(), pairs...)
}
