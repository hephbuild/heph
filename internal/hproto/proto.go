package hproto

import (
	"fmt"
	"slices"
	"strings"

	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/reflect/protopath"
	"google.golang.org/protobuf/reflect/protorange"
	"google.golang.org/protobuf/reflect/protoreflect"
)

func Clone[T proto.Message](m T) T {
	return proto.Clone(m).(T) //nolint:errcheck
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

func RemoveMasked[T proto.Message](m T, paths []string) (T, error) {
	m = Clone(m)

	err := protorange.Range(m.ProtoReflect(), func(p protopath.Values) error {
		if !slices.Contains(paths, protoPathValueToDotPath(p)) {
			return nil
		}

		last := p.Index(-1)

		beforeLast := p.Index(-2)
		switch last.Step.Kind() { //nolint:exhaustive
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
