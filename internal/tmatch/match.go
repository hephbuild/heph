package tmatch

import (
	"context"
	"io/fs"
	"iter"
	"path/filepath"
	"slices"
	"strings"

	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hsingleflight"
	"github.com/hephbuild/heph/lib/tref"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

type cachedFs struct {
	fs.ReadDirFS

	memReadDir hsingleflight.GroupMem[string, []fs.DirEntry]
}

func (c *cachedFs) ReadDir(name string) ([]fs.DirEntry, error) {
	entries, err, _ := c.memReadDir.Do(name, func() ([]fs.DirEntry, error) {
		return c.ReadDirFS.ReadDir(name)
	})

	return entries, err
}

// TODO: move to context or something like that
var fsWalkCache = hfs.NewFSCache()

func walkDirs(ctx context.Context, walkRoot, root string, filter func(path string) bool) iter.Seq2[string, error] {
	if filter == nil {
		filter = func(path string) bool {
			return true
		}
	}

	return func(yield func(string, error) bool) {
		err := fsWalkCache.Walk(walkRoot, func(path string, d fs.DirEntry, err error) error {
			if d == nil || !d.IsDir() {
				return nil
			}

			if err := ctx.Err(); err != nil {
				return err
			}

			if err != nil {
				return err
			}

			if !filter(path) {
				return fs.SkipDir
			}

			pkg, err := tref.DirToPackage(path, root)
			if err != nil {
				return err
			}

			if !yield(pkg, nil) {
				return fs.SkipAll
			}

			return nil
		})
		if err != nil {
			if !yield("", err) {
				return
			}
		}
	}
}

type PackageProvider = func(ctx context.Context, basePkg string) iter.Seq2[string, error]

func OSPackageProvider(root string, filter func(path string) bool) PackageProvider {
	return func(ctx context.Context, basePkg string) iter.Seq2[string, error] {
		walkRoot := filepath.Join(root, tref.ToOSPath(basePkg))

		return walkDirs(ctx, walkRoot, root, filter)
	}
}

func Packages(ctx context.Context, p PackageProvider, m *pluginv1.TargetMatcher) iter.Seq2[string, error] {
	return func(yield func(string, error) bool) {
		basePkg, _ := extractRoot(m)

		for pkg, err := range p(ctx, basePkg) {
			if err != nil {
				yield("", err)
				return
			}

			if MatchPackage(pkg, m) == MatchNo {
				continue
			}

			if !yield(pkg, nil) {
				return
			}
		}
	}
}

func ParsePackageMatcher(pkg, cwd, root string) (*pluginv1.TargetMatcher, error) {
	if pkg == "..." {
		cwp, err := tref.DirToPackage(cwd, root)
		if err != nil {
			return nil, err
		}

		return PackagePrefix(tref.JoinPackage(cwp)), nil
	}

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
		return PackagePrefix(pkg), nil
	} else {
		return Package(pkg), nil
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
	if m == nil {
		return MatchNo
	}

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
		if !tref.HasPackagePrefix(m.GetCodegenPackage(), pkg) {
			return MatchNo
		}

		return MatchShrug
	case pluginv1.TargetMatcher_Or_case:
		return runOr(m, func(m *pluginv1.TargetMatcher) Result {
			return MatchPackage(pkg, m)
		})
	case pluginv1.TargetMatcher_And_case:
		return runAnd(m, func(m *pluginv1.TargetMatcher) Result {
			return MatchPackage(pkg, m)
		})
	case pluginv1.TargetMatcher_Not_case:
		return runNot(m, func(m *pluginv1.TargetMatcher) Result {
			return MatchPackage(pkg, m)
		})
	default:
		panic("unhandled target matcher type: " + m.String())
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
		return runOr(m, func(m *pluginv1.TargetMatcher) Result {
			return MatchSpec(spec, m)
		})
	case pluginv1.TargetMatcher_And_case:
		return runAnd(m, func(m *pluginv1.TargetMatcher) Result {
			return MatchSpec(spec, m)
		})
	case pluginv1.TargetMatcher_Not_case:
		return runNot(m, func(m *pluginv1.TargetMatcher) Result {
			return MatchSpec(spec, m)
		})
	default:
		panic("unhandled target matcher type: " + m.String())
	}
}

func runAnd(m *pluginv1.TargetMatcher, fn func(m *pluginv1.TargetMatcher) Result) Result {
	out := MatchYes
	for _, matcher := range m.GetAnd().GetItems() {
		switch fn(matcher) {
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
}

func runOr(m *pluginv1.TargetMatcher, fn func(m *pluginv1.TargetMatcher) Result) Result {
	out := MatchNo
	for _, matcher := range m.GetOr().GetItems() {
		switch fn(matcher) {
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
}

func runNot(m *pluginv1.TargetMatcher, fn func(m *pluginv1.TargetMatcher) Result) Result {
	switch fn(m.GetNot()) {
	case MatchYes:
		return MatchNo
	case MatchNo:
		return MatchYes
	case MatchShrug:
		return MatchShrug
	default:
		panic("unhandled result")
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
		if !tref.HasPackagePrefix(m.GetCodegenPackage(), spec.GetRef().GetPackage()) {
			return MatchNo
		}

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
		return runOr(m, func(m *pluginv1.TargetMatcher) Result {
			return MatchDef(spec, def, m)
		})
	case pluginv1.TargetMatcher_And_case:
		return runAnd(m, func(m *pluginv1.TargetMatcher) Result {
			return MatchDef(spec, def, m)
		})
	case pluginv1.TargetMatcher_Not_case:
		return runNot(m, func(m *pluginv1.TargetMatcher) Result {
			return MatchDef(spec, def, m)
		})
	default:
		panic("unhandled target matcher type: " + m.String())
	}
}

func extractRoot(m *pluginv1.TargetMatcher) (string, bool) {
	if m == nil {
		return "", false
	}

	switch m.WhichItem() {
	case pluginv1.TargetMatcher_Ref_case:
		return m.GetRef().GetPackage(), true
	case pluginv1.TargetMatcher_Package_case:
		return m.GetPackage(), true
	case pluginv1.TargetMatcher_PackagePrefix_case:
		return m.GetPackagePrefix(), true
	case pluginv1.TargetMatcher_Label_case:
		return "", false
	case pluginv1.TargetMatcher_CodegenPackage_case:
		return "", false
	case pluginv1.TargetMatcher_Or_case:
		var roots []string
		for _, matcher := range m.GetOr().GetItems() {
			r, ok := extractRoot(matcher)
			if !ok {
				continue
			}

			if slices.Contains(roots, r) {
				continue
			}

			roots = append(roots, r)
		}

		return longestCommonPath(roots)
	case pluginv1.TargetMatcher_And_case:
		var roots []string
		for _, matcher := range m.GetAnd().GetItems() {
			r, ok := extractRoot(matcher)
			if !ok {
				continue
			}

			if slices.Contains(roots, r) {
				continue
			}

			roots = append(roots, r)
		}

		return longestCommonPath(roots)
	case pluginv1.TargetMatcher_Not_case:
		return "", false
	default:
		panic("unhandled target matcher type: " + m.String())
	}
}

func longestCommonPath(paths []string) (string, bool) {
	switch len(paths) {
	case 0:
		return "", false
	case 1:
		return paths[0], true
	}

	// 1. Find the raw string common prefix (standard horizontal scanning)
	prefix := paths[0]
	for _, path := range paths[1:] {
		for !strings.HasPrefix(path, prefix) {
			prefix = prefix[:len(prefix)-1]
			if prefix == "" {
				return "", true
			}
		}
	}

	// 2. Verify Segment Integrity
	// The prefix might be "/usr/bin" derived from ["/usr/bin", "/usr/binder"].
	// We must ensure this prefix is a valid directory boundary for ALL paths.
	for _, path := range paths {
		// Check if the path continues after the prefix.
		// If it does, the next character MUST be a '/' separator.
		// If it is not, we have cut a word in half (e.g., matching "bin" inside "binder").
		if len(path) > len(prefix) && path[len(prefix)] != '/' {
			// We are mid-segment. Cut back to the last separator.
			lastSlash := strings.LastIndex(prefix, "/")
			if lastSlash == -1 {
				return "", true
			}
			prefix = prefix[:lastSlash]

			// Once we truncate, we are guaranteed to be safe because the shorter
			// prefix matches the parent directory, which is common to all.
			break
		}
	}

	return prefix, true
}
