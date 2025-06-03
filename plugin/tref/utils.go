package tref

import (
	"strings"
)

func Equal(a, b Ref) bool {
	if a.GetName() != b.GetName() {
		return false
	}
	if a.GetPackage() != b.GetPackage() {
		return false
	}
	if a.GetArgsCount() != b.GetArgsCount() {
		return false
	}
	for k, av := range a.GetArgs() {
		bv, ok := b.GetArg(k)
		if !ok {
			return false
		}

		if av != bv {
			return false
		}
	}

	return true
}

func CompareOut(a, b RefWithOutput) int {
	if v := Compare(WithoutOut(a), WithoutOut(b)); v != 0 {
		return v
	}

	if a.GetOutput() != nil && b.GetOutput() != nil {
		if v := strings.Compare(*a.GetOutput(), *b.GetOutput()); v != 0 {
			return v
		}
	} else {
		if a.GetOutput() == nil {
			return 1
		}
		if b.GetOutput() == nil {
			return -1
		}
	}

	return 0
}

func Compare(a, b Ref) int {
	if v := strings.Compare(a.GetPackage(), b.GetPackage()); v != 0 {
		return v
	}

	if v := strings.Compare(a.GetName(), b.GetName()); v != 0 {
		return v
	}

	if v := a.GetArgsCount() - b.GetArgsCount(); v != 0 {
		return v
	}

	for k, av := range a.GetArgs() {
		bv, ok := b.GetArg(k)
		if !ok {
			return 1
		}

		if v := strings.Compare(av, bv); v != 0 {
			return v
		}
	}

	return 0
}

func WithName(ref Ref, name string) Ref {
	if ref.GetName() == name {
		return ref
	}

	ref.name = name

	return ref
}

func WithArg(ref Ref, key, value string) Ref {
	ref.SetArg(key, value)
	return ref
}

func WithArgs(ref Ref, m map[string]string) Ref {
	ref.setArgs(m)

	return ref
}

func WithOut(ref Ref, output string) RefWithOutput {
	return RefWithOutput{
		Ref:     ref,
		out:     output,
		filters: nil,
	}
}

func WithFilters(ref RefWithOutput, filters []string) RefWithOutput {
	ref.filters = filters

	return ref
}

func WithoutOut(ref RefWithOutput) Ref {
	return ref.Ref
}
