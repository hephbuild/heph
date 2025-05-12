package tref

import (
	"strings"

	"github.com/hephbuild/heph/internal/hproto"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"google.golang.org/protobuf/proto"
)

func Equal(a, b *pluginv1.TargetRef) bool {
	return proto.Equal(a, b)
}

func CompareOut(a, b *pluginv1.TargetRefWithOutput) int {
	if v := Compare(WithoutOut(a), WithoutOut(b)); v != 0 {
		return v
	}

	if a.Output != nil && b.Output != nil {
		if v := strings.Compare(a.GetOutput(), b.GetOutput()); v != 0 {
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

func Compare(a, b *pluginv1.TargetRef) int {
	if v := strings.Compare(a.GetPackage(), b.GetPackage()); v != 0 {
		return v
	}

	if v := strings.Compare(a.GetName(), b.GetName()); v != 0 {
		return v
	}

	if v := len(a.GetArgs()) - len(b.GetArgs()); v != 0 {
		return v
	}

	for k, av := range a.GetArgs() {
		bv, ok := b.GetArgs()[k]
		if !ok {
			return 1
		}

		if v := strings.Compare(av, bv); v != 0 {
			return v
		}
	}

	return 0
}

func WithName(ref *pluginv1.TargetRef, name string) *pluginv1.TargetRef {
	if ref.GetName() == name {
		return ref
	}

	ref = hproto.Clone(ref)
	ref.Name = name
	return ref
}

func WithArg(ref *pluginv1.TargetRef, key, value string) *pluginv1.TargetRef {
	if ref.Args != nil && ref.GetArgs()[key] == value {
		return ref
	}

	ref = hproto.Clone(ref)
	if ref.Args == nil {
		ref.Args = make(map[string]string)
	}
	ref.Args[key] = value
	return ref
}

func WithArgs(ref *pluginv1.TargetRef, m map[string]string) *pluginv1.TargetRef {
	if len(m) == 0 && len(ref.Args) == 0 {
		return ref
	}

	ref = hproto.Clone(ref)
	ref.Args = m
	return ref
}

func WithOut(ref *pluginv1.TargetRef, output string) *pluginv1.TargetRefWithOutput {
	var outputp *string
	if output != "" {
		outputp = &output
	}

	return &pluginv1.TargetRefWithOutput{
		Package: ref.GetPackage(),
		Name:    ref.GetName(),
		Args:    ref.GetArgs(),
		Output:  outputp,
	}
}

func WithFilters(ref *pluginv1.TargetRefWithOutput, filters []string) *pluginv1.TargetRefWithOutput {
	ref = hproto.Clone(ref)
	ref.Filters = filters

	return ref
}

func WithoutOut(ref *pluginv1.TargetRefWithOutput) *pluginv1.TargetRef {
	return &pluginv1.TargetRef{
		Package: ref.GetPackage(),
		Name:    ref.GetName(),
		Args:    ref.GetArgs(),
	}
}
