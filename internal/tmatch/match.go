package tmatch

import (
	"context"
	"errors"
	"fmt"
	"io/fs"
	"iter"
	"os"
	"path/filepath"
	"slices"
	"strings"

	"github.com/hephbuild/heph/lib/tref"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"google.golang.org/protobuf/proto"
)

func Packages(ctx context.Context, root string, m *pluginv1.TargetMatcher, filter func(path string) bool) iter.Seq2[string, error] {
	if filter == nil {
		filter = func(path string) bool {
			return true
		}
	}

	walkRoot := extractRoot(root, m)

	return func(yield func(string, error) bool) {
		err := fs.WalkDir(os.DirFS(walkRoot), ".", func(path string, d fs.DirEntry, err error) error {
			if d == nil || !d.IsDir() {
				return nil
			}

			if err := ctx.Err(); err != nil {
				return err
			}

			path = filepath.Join(walkRoot, path)

			if !filter(path) {
				return fs.SkipDir
			}

			if err != nil {
				return err
			}

			pkg, err := tref.DirToPackage(path, root)
			if err != nil {
				return err
			}

			if MatchPackage(pkg, m) == MatchNo {
				return nil
			}

			if !yield(pkg, nil) {
				return fs.SkipAll
			}

			return nil
		})
		if err != nil {
			if errors.Is(err, os.ErrNotExist) {
				err = fmt.Errorf("%v doesnt exist", walkRoot)
			}

			yield("", err)
		}
	}
}

func ParsePackageMatcher(pkg, cwd, root string) (*pluginv1.TargetMatcher, error) {
	pkg = strings.TrimPrefix(pkg, "//")

	prefix := false
	if rest, ok := strings.CutSuffix(pkg, "..."); ok {
		pkg = strings.TrimSuffix(rest, "/")
		prefix = true
	}

	if rest, ok := strings.CutPrefix(pkg, "."); ok {
		rest = strings.TrimPrefix(rest, "/")

		cwp, err := tref.DirToPackage(cwd, root)
		if err != nil {
			return nil, err
		}

		pkg = tref.JoinPackage(cwp, rest)
	}

	if prefix {
		return pluginv1.TargetMatcher_builder{PackagePrefix: proto.String(pkg)}.Build(), nil
	} else {
		return pluginv1.TargetMatcher_builder{Package: proto.String(pkg)}.Build(), nil
	}
}

type Result int

func (r Result) Definitive() bool {
	if r == 0 {
		panic("invalid result")
	}

	return r != MatchShrug
}

func boolToResult(b bool) Result {
	if b {
		return MatchYes
	} else {
		return MatchNo
	}
}

const (
	MatchYes   Result = 1
	MatchNo    Result = 2
	MatchShrug Result = -1
)

func MatchPackage(pkg string, m *pluginv1.TargetMatcher) Result {
	switch m.WhichItem() {
	case pluginv1.TargetMatcher_Ref_case:
		return boolToResult(m.GetRef().GetPackage() == pkg)
	case pluginv1.TargetMatcher_Package_case:
		return boolToResult(m.GetPackage() == pkg)
	case pluginv1.TargetMatcher_PackagePrefix_case:
		return boolToResult(tref.HasPackagePrefix(pkg, m.GetPackagePrefix()))
	case pluginv1.TargetMatcher_Label_case:
		return MatchShrug
	case pluginv1.TargetMatcher_CodegenPackage_case:
		if !tref.HasPackagePrefix(pkg, m.GetCodegenPackage()) {
			return MatchNo
		}

		return MatchShrug
	case pluginv1.TargetMatcher_Or_case:
		out := MatchNo
		for _, matcher := range m.GetOr().GetItems() {
			switch MatchPackage(pkg, matcher) {
			case MatchYes:
				out = MatchYes
			case MatchNo:
				// dont touch
			case MatchShrug:
				return MatchShrug
			default:
				panic("unhandled result")
			}
		}

		return out
	case pluginv1.TargetMatcher_And_case:
		out := MatchYes
		for _, matcher := range m.GetAnd().GetItems() {
			switch MatchPackage(pkg, matcher) {
			case MatchYes:
				// dont touch
			case MatchNo:
				return MatchNo
			case MatchShrug:
				out = MatchShrug
			default:
				panic("unhandled result")
			}
		}

		return out
	case pluginv1.TargetMatcher_Not_case:
		switch MatchPackage(pkg, m.GetNot()) {
		case MatchYes:
			return MatchNo
		case MatchNo:
			return MatchYes
		case MatchShrug:
			return MatchShrug
		default:
			panic("unhandled result")
		}
	default:
		panic("unhandled target matcher type")
	}
}

func MatchSpec(spec *pluginv1.TargetSpec, m *pluginv1.TargetMatcher) Result {
	if m == nil {
		return MatchNo
	}

	switch m.WhichItem() {
	case pluginv1.TargetMatcher_Ref_case:
		return boolToResult(tref.Equal(m.GetRef(), spec.GetRef()))
	case pluginv1.TargetMatcher_Package_case:
		return boolToResult(m.GetPackage() == spec.GetRef().GetPackage())
	case pluginv1.TargetMatcher_PackagePrefix_case:
		return boolToResult(tref.HasPackagePrefix(spec.GetRef().GetPackage(), m.GetPackagePrefix()))
	case pluginv1.TargetMatcher_Label_case:
		return boolToResult(slices.Contains(spec.GetLabels(), m.GetLabel()))
	case pluginv1.TargetMatcher_CodegenPackage_case:
		if !tref.HasPackagePrefix(m.GetCodegenPackage(), spec.GetRef().GetPackage()) {
			return MatchNo
		}

		return MatchShrug
	case pluginv1.TargetMatcher_Or_case:
		out := MatchNo
		for _, matcher := range m.GetOr().GetItems() {
			switch MatchSpec(spec, matcher) {
			case MatchYes:
				out = MatchYes
			case MatchNo:
				// dont touch
			case MatchShrug:
				return MatchShrug
			default:
				panic("unhandled result")
			}
		}

		return out
	case pluginv1.TargetMatcher_And_case:
		out := MatchYes
		for _, matcher := range m.GetAnd().GetItems() {
			switch MatchSpec(spec, matcher) {
			case MatchYes:
				// dont touch
			case MatchNo:
				return MatchNo
			case MatchShrug:
				out = MatchShrug
			default:
				panic("unhandled result")
			}
		}

		return out
	case pluginv1.TargetMatcher_Not_case:
		switch MatchSpec(spec, m.GetNot()) {
		case MatchYes:
			return MatchNo
		case MatchNo:
			return MatchYes
		case MatchShrug:
			return MatchShrug
		default:
			panic("unhandled result")
		}
	default:
		panic("unhandled target matcher type")
	}
}

func MatchDef(spec *pluginv1.TargetSpec, def *pluginv1.TargetDef, m *pluginv1.TargetMatcher) Result {
	if m == nil {
		return MatchNo
	}

	switch m.WhichItem() {
	case pluginv1.TargetMatcher_Ref_case:
		return boolToResult(tref.Equal(m.GetRef(), def.GetRef()))
	case pluginv1.TargetMatcher_Package_case:
		return boolToResult(m.GetPackage() == def.GetRef().GetPackage())
	case pluginv1.TargetMatcher_PackagePrefix_case:
		return boolToResult(tref.HasPackagePrefix(def.GetRef().GetPackage(), m.GetPackagePrefix()))
	case pluginv1.TargetMatcher_Label_case:
		return boolToResult(slices.Contains(spec.GetLabels(), m.GetLabel()))
	case pluginv1.TargetMatcher_CodegenPackage_case:
		if len(def.GetCodegenTree()) == 0 {
			return MatchNo
		}

		for _, gen := range def.GetCodegenTree() {
			if gen.GetIsDir() {
				outPkg := tref.JoinPackage(def.GetRef().GetPackage(), tref.ToPackage(gen.GetPath()))
				if tref.HasPackagePrefix(outPkg, m.GetCodegenPackage()) {
					return MatchYes
				}
			} else {
				outPkg := tref.JoinPackage(def.GetRef().GetPackage(), tref.ToPackage(filepath.Dir(gen.GetPath())))
				if outPkg == m.GetCodegenPackage() {
					return MatchYes
				}
			}
		}

		return MatchNo
	case pluginv1.TargetMatcher_Or_case:
		out := MatchNo
		for _, matcher := range m.GetOr().GetItems() {
			switch MatchDef(spec, def, matcher) {
			case MatchYes:
				out = MatchYes
			case MatchNo:
				// dont touch
			case MatchShrug:
				return MatchShrug
			default:
				panic("unhandled result")
			}
		}

		return out
	case pluginv1.TargetMatcher_And_case:
		out := MatchYes
		for _, matcher := range m.GetAnd().GetItems() {
			switch MatchDef(spec, def, matcher) {
			case MatchYes:
				// dont touch
			case MatchNo:
				return MatchNo
			case MatchShrug:
				out = MatchShrug
			default:
				panic("unhandled result")
			}
		}

		return out
	case pluginv1.TargetMatcher_Not_case:
		switch MatchDef(spec, def, m.GetNot()) {
		case MatchYes:
			return MatchNo
		case MatchNo:
			return MatchYes
		case MatchShrug:
			return MatchShrug
		default:
			panic("unhandled result")
		}
	default:
		panic("unhandled target matcher type")
	}
}

func extractRoot(root string, m *pluginv1.TargetMatcher) string {
	if m == nil {
		return root
	}

	switch m.WhichItem() {
	case pluginv1.TargetMatcher_Ref_case:
		return filepath.Join(root, tref.ToOSPath(m.GetRef().GetPackage()))
	case pluginv1.TargetMatcher_Package_case:
		return filepath.Join(root, tref.ToOSPath(m.GetPackage()))
	case pluginv1.TargetMatcher_PackagePrefix_case:
		return filepath.Join(root, tref.ToOSPath(m.GetPackagePrefix()))
	case pluginv1.TargetMatcher_Label_case:
		return root
	case pluginv1.TargetMatcher_CodegenPackage_case:
		return root
	case pluginv1.TargetMatcher_Or_case:
		var roots []string
		for _, matcher := range m.GetOr().GetItems() {
			r := extractRoot(root, matcher)
			if r == root {
				continue
			}

			roots = append(roots, r)
		}

		if len(roots) == 1 {
			return roots[0]
		}

		return root // TODO array
	case pluginv1.TargetMatcher_And_case:
		var roots []string
		for _, matcher := range m.GetAnd().GetItems() {
			r := extractRoot(root, matcher)
			if r == root {
				continue
			}

			roots = append(roots, r)
		}

		if len(roots) == 1 {
			return roots[0]
		}

		return root // TODO find smallest denominator
	case pluginv1.TargetMatcher_Not_case:
		return root
	default:
		panic("unhandled target matcher type")
	}
}
