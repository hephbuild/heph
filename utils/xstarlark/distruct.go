package xstarlark

import (
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"go.starlark.net/starlark"
	"go.starlark.net/starlarkstruct"
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

func kvFrom(v starlark.Value) (KV, bool) {
	switch v := v.(type) {
	case *starlark.Dict:
		kv := &dictkv{}
		kv.dict = v
		return kv, true
	case *starlarkstruct.Struct:
		kv := &structkv{}
		kv.s = v
		return kv, true
	default:
		return nil, false
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
	kv, ok := kvFrom(v)
	if !ok {
		return fmt.Errorf("must be a struct or dict")
	}

	s.items = kv.Items()
	return nil
}
