package tref

import (
	"github.com/hephbuild/heph/internal/hproto"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"google.golang.org/protobuf/proto"
	"strings"
)

func Equal(a, b *pluginv1.TargetRef) bool {
	a = WithDriver(a, "")
	b = WithDriver(b, "")

	return proto.Equal(a, b)
}

func CompareOut(a, b *pluginv1.TargetRefWithOutput) int {
	if v := strings.Compare(a.Package, b.Package); v != 0 {
		return v
	}

	if v := strings.Compare(a.Name, b.Name); v != 0 {
		return v
	}

	if v := len(a.Args) - len(b.Args); v != 0 {
		return v
	}

	for k, av := range a.Args {
		bv, ok := b.Args[k]
		if !ok {
			return 1
		}

		if v := strings.Compare(av, bv); v != 0 {
			return v
		}
	}

	if a.Output != nil && b.Output != nil {
		if v := strings.Compare(*a.Output, *b.Output); v != 0 {
			return v
		}
	} else {
		if a.Output == nil {
			return 1
		}
		if b.Output == nil {
			return -1
		}
	}

	return 0
}

func WithDriver(ref *pluginv1.TargetRef, driver string) *pluginv1.TargetRef {
	if ref.Driver == driver {
		return ref
	}

	ref = hproto.Clone(ref)
	ref.Driver = driver
	return ref
}

func WithName(ref *pluginv1.TargetRef, name string) *pluginv1.TargetRef {
	if ref.Name == name {
		return ref
	}

	ref = hproto.Clone(ref)
	ref.Name = name
	return ref
}

func WithArg(ref *pluginv1.TargetRef, key, value string) *pluginv1.TargetRef {
	if ref.Args != nil && ref.Args[key] == value {
		return ref
	}

	ref = hproto.Clone(ref)
	if ref.Args == nil {
		ref.Args = make(map[string]string)
	}
	ref.Args[key] = value
	return ref
}

func WithOut(ref *pluginv1.TargetRef, output string) *pluginv1.TargetRefWithOutput {
	return &pluginv1.TargetRefWithOutput{
		Package: ref.Package,
		Name:    ref.Name,
		Driver:  ref.Driver,
		Args:    ref.Args,
		Output:  &output,
	}
}

func WithoutOut(ref *pluginv1.TargetRefWithOutput) *pluginv1.TargetRef {
	return &pluginv1.TargetRef{
		Package: ref.Package,
		Name:    ref.Name,
		Driver:  ref.Driver,
		Args:    ref.Args,
	}
}
