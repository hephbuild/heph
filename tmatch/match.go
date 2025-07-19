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

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
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
		return &pluginv1.TargetMatcher{Item: &pluginv1.TargetMatcher_PackagePrefix{PackagePrefix: pkg}}, nil
	} else {
		return &pluginv1.TargetMatcher{Item: &pluginv1.TargetMatcher_Package{Package: pkg}}, nil
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
	switch item := m.GetItem().(type) {
	case *pluginv1.TargetMatcher_Ref:
		return boolToResult(item.Ref.GetPackage() == pkg)
	case *pluginv1.TargetMatcher_Package:
		return boolToResult(item.Package == pkg)
	case *pluginv1.TargetMatcher_PackagePrefix:
		return boolToResult(tref.HasPackagePrefix(pkg, item.PackagePrefix))
	case *pluginv1.TargetMatcher_Label:
		return MatchShrug
	case *pluginv1.TargetMatcher_CodegenPackage:
		if !tref.HasPackagePrefix(pkg, item.CodegenPackage) {
			return MatchNo
		}

		return MatchShrug
	case *pluginv1.TargetMatcher_Or:
		out := MatchNo
		for _, matcher := range item.Or.GetItems() {
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
	case *pluginv1.TargetMatcher_And:
		out := MatchYes
		for _, matcher := range item.And.GetItems() {
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
	case *pluginv1.TargetMatcher_Not:
		switch MatchPackage(pkg, item.Not) {
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

	switch item := m.GetItem().(type) {
	case *pluginv1.TargetMatcher_Ref:
		return boolToResult(tref.Equal(item.Ref, spec.GetRef()))
	case *pluginv1.TargetMatcher_Package:
		return boolToResult(item.Package == spec.GetRef().GetPackage())
	case *pluginv1.TargetMatcher_PackagePrefix:
		return boolToResult(tref.HasPackagePrefix(spec.GetRef().GetPackage(), item.PackagePrefix))
	case *pluginv1.TargetMatcher_Label:
		return boolToResult(slices.Contains(spec.GetLabels(), item.Label))
	case *pluginv1.TargetMatcher_CodegenPackage:
		if !tref.HasPackagePrefix(item.CodegenPackage, spec.GetRef().GetPackage()) {
			return MatchNo
		}

		return MatchShrug
	case *pluginv1.TargetMatcher_Or:
		out := MatchNo
		for _, matcher := range item.Or.GetItems() {
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
	case *pluginv1.TargetMatcher_And:
		out := MatchYes
		for _, matcher := range item.And.GetItems() {
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
	case *pluginv1.TargetMatcher_Not:
		switch MatchSpec(spec, item.Not) {
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

	switch item := m.GetItem().(type) {
	case *pluginv1.TargetMatcher_Ref:
		return boolToResult(tref.Equal(item.Ref, def.GetRef()))
	case *pluginv1.TargetMatcher_Package:
		return boolToResult(item.Package == def.GetRef().GetPackage())
	case *pluginv1.TargetMatcher_PackagePrefix:
		return boolToResult(tref.HasPackagePrefix(def.GetRef().GetPackage(), item.PackagePrefix))
	case *pluginv1.TargetMatcher_Label:
		return boolToResult(slices.Contains(spec.GetLabels(), item.Label))
	case *pluginv1.TargetMatcher_CodegenPackage:
		if def.GetCodegenTree() == nil || def.GetCodegenTree().GetMode() == pluginv1.TargetDef_CodegenTree_CODEGEN_MODE_UNSPECIFIED {
			return MatchNo
		}

		for _, path := range def.GetCodegenTree().GetPaths() {
			outPkg := tref.JoinPackage(def.GetRef().GetPackage(), tref.ToPackage(path))
			// TODO: differentiate file from dir output
			if tref.HasPackagePrefix(outPkg, item.CodegenPackage) {
				return MatchYes
			}
		}

		return MatchNo
	case *pluginv1.TargetMatcher_Or:
		out := MatchNo
		for _, matcher := range item.Or.GetItems() {
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
	case *pluginv1.TargetMatcher_And:
		out := MatchYes
		for _, matcher := range item.And.GetItems() {
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
	case *pluginv1.TargetMatcher_Not:
		switch MatchDef(spec, def, item.Not) {
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

	switch item := m.GetItem().(type) {
	case *pluginv1.TargetMatcher_Ref:
		return filepath.Join(root, tref.ToOSPath(item.Ref.GetPackage()))
	case *pluginv1.TargetMatcher_Package:
		return filepath.Join(root, tref.ToOSPath(item.Package))
	case *pluginv1.TargetMatcher_PackagePrefix:
		return filepath.Join(root, tref.ToOSPath(item.PackagePrefix))
	case *pluginv1.TargetMatcher_Label:
		return root
	case *pluginv1.TargetMatcher_CodegenPackage:
		return root
	case *pluginv1.TargetMatcher_Or:
		var roots []string
		for _, matcher := range item.Or.GetItems() {
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
	case *pluginv1.TargetMatcher_And:
		var roots []string
		for _, matcher := range item.And.GetItems() {
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
	case *pluginv1.TargetMatcher_Not:
		return root
	default:
		panic("unhandled target matcher type")
	}
}
