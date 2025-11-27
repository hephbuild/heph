package tref

import (
	"maps"
	"strings"

	"github.com/hephbuild/heph/internal/htypes"

	"github.com/hephbuild/heph/internal/hproto"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func Equal(a, b *pluginv1.TargetRef) bool {
	return a.GetPackage() == b.GetPackage() && a.GetName() == b.GetName() && maps.Equal(a.GetArgs(), b.GetArgs())
}

func CompareOut(a, b *pluginv1.TargetRefWithOutput) int {
	if v := Compare(WithoutOut(a), WithoutOut(b)); v != 0 {
		return v
	}

	if a.HasOutput() && b.HasOutput() {
		if v := strings.Compare(a.GetOutput(), b.GetOutput()); v != 0 {
			return v
		}
	} else {
		if !a.HasOutput() {
			return 1
		}
		if !b.HasOutput() {
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

func New(pkg, name string, args map[string]string) *pluginv1.TargetRef {
	return pluginv1.TargetRef_builder{
		Package: htypes.Ptr(pkg),
		Name:    htypes.Ptr(name),
		Args:    args,
	}.Build()
}

func WithName(ref *pluginv1.TargetRef, name string) *pluginv1.TargetRef {
	if ref.GetName() == name {
		return ref
	}

	ref = hproto.Clone(ref)
	ref.ClearHash()
	ref.SetName(name)
	return ref
}

func WithArg(ref *pluginv1.TargetRef, key, value string) *pluginv1.TargetRef {
	if ref.GetArgs() != nil && ref.GetArgs()[key] == value {
		return ref
	}

	ref = hproto.Clone(ref)
	ref.ClearHash()
	if ref.GetArgs() == nil {
		ref.SetArgs(make(map[string]string))
	}
	ref.GetArgs()[key] = value
	return ref
}

func WithArgs(ref *pluginv1.TargetRef, m map[string]string) *pluginv1.TargetRef {
	if len(m) == 0 && len(ref.GetArgs()) == 0 {
		return ref
	}

	ref = hproto.Clone(ref)
	ref.ClearHash()
	ref.SetArgs(m)
	return ref
}

func WithOut(ref *pluginv1.TargetRef, output string) *pluginv1.TargetRefWithOutput {
	var outputp *string
	if output != "" {
		outputp = &output
	}

	return pluginv1.TargetRefWithOutput_builder{
		Package: htypes.Ptr(ref.GetPackage()),
		Name:    htypes.Ptr(ref.GetName()),
		Args:    ref.GetArgs(),
		Output:  outputp,
	}.Build()
}

func WithFilters(ref *pluginv1.TargetRefWithOutput, filters []string) *pluginv1.TargetRefWithOutput {
	ref = hproto.Clone(ref)
	ref.SetFilters(filters)

	return ref
}

func WithoutOut(ref *pluginv1.TargetRefWithOutput) *pluginv1.TargetRef {
	return New(ref.GetPackage(), ref.GetName(), ref.GetArgs())
}
