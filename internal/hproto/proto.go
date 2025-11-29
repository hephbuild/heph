package hproto

import (
	"fmt"
	"strings"

	"github.com/hephbuild/heph/internal/hproto/hashpb"

	"github.com/zeebo/xxh3"

	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/reflect/protopath"
	"google.golang.org/protobuf/reflect/protorange"
	"google.golang.org/protobuf/reflect/protoreflect"
)

func Clone[T proto.Message](m T) T {
	return proto.CloneOf(m)
}

func Equal[T proto.Message](a, b T) bool {
	return proto.Equal(a, b)
}

func protoPathValueToDotPath(p protopath.Values) string {
	segments := make([]string, 0, len(p.Path))
	for _, step := range p.Path {
		switch step.Kind() { //nolint:exhaustive,gocritic
		case protopath.FieldAccessStep:
			segments = append(segments, step.FieldDescriptor().TextName())
		}
	}

	return strings.Join(segments, ".")
}

func RemoveMasked[T proto.Message](m T, paths map[string]struct{}) (T, error) {
	if len(paths) == 0 {
		return m, nil
	}

	m = Clone(m)

	err := protorange.Range(m.ProtoReflect(), func(p protopath.Values) error {
		if _, ok := paths[protoPathValueToDotPath(p)]; !ok {
			return nil
		}

		last := p.Index(-1)

		beforeLast := p.Index(-2)
		switch last.Step.Kind() {
		case protopath.FieldAccessStep:
			m := beforeLast.Value.Message()
			fd := last.Step.FieldDescriptor()
			m.Clear(fd)
		case protopath.ListIndexStep:
			ls := beforeLast.Value.List()
			i := last.Step.ListIndex()
			// TODO: figure out how to remove
			ls.Set(i, protoreflect.ValueOfMessage(ls.Get(i).Message().Type().New()))
		case protopath.MapIndexStep:
			ms := beforeLast.Value.Map()
			k := last.Step.MapIndex()
			ms.Clear(k)
		default:
			return fmt.Errorf("unsupported field access step: %v", last.Step.Kind())
		}

		return nil
	})

	return m, err
}

func Compare(a, b hashpb.StableWriter, omit map[string]struct{}) int {
	ha := xxh3.New()
	hashpb.Hash(ha, a, omit)

	hb := xxh3.New()
	hashpb.Hash(hb, b, omit)

	suma := ha.Sum64()
	sumb := hb.Sum64()

	switch {
	case suma < sumb:
		return -1
	case suma > sumb:
		return 1
	default:
		return 0
	}
}

func New[T proto.Message]() T {
	var v T
	return v.ProtoReflect().New().Interface().(T) //nolint:errcheck
}
