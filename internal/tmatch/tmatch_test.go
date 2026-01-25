package tmatch

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hslices"
	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/stretchr/testify/assert"

	"github.com/stretchr/testify/require"
)

func TestParsePackageMatcher(t *testing.T) {
	tests := []struct {
		s        string
		root     string
		cwd      string
		expected string
	}{
		{"./...", "/root", "/root", `package_prefix:""`},
		{"./...", "/root", "/root/foo", `package_prefix:"foo"`},
		{"...", "/root", "/root", `package_prefix:""`},
		{"...", "/root", "/root/foo", `package_prefix:"foo"`},
		{"//...", "/root", "/root/foo", `package_prefix:""`},
		{".", "/root", "/root/foo", `package:"foo"`},
		{"./foo", "/root", "/root", `package:"foo"`},
		{"foo", "/root", "/root", `package:"foo"`},
		{"foo", "/root", "/root/foo", `package:"foo"`},
	}
	for _, test := range tests {
		t.Run(test.s+" "+test.root+" "+test.cwd+" "+test.expected, func(t *testing.T) {
			m, err := ParsePackageMatcher(test.s, test.cwd, test.root)
			require.NoError(t, err)

			require.Equal(t, test.expected, m.String())
		})
	}
}

func TestMatchCodegenPackage(t *testing.T) {
	tests := []struct {
		pkg        string
		codegenPkg string
		expected   Result
	}{
		{"foo/bar", "foo/bar", MatchShrug},
		{"foo", "foo/bar", MatchShrug},
		{"", "foo/bar", MatchShrug},
		{"foo/bar/baz", "foo/bar", MatchNo},
		{"unrelated/bar", "foo/bar", MatchNo},
	}
	for _, test := range tests {
		t.Run("MatchPackage "+test.pkg+" "+test.codegenPkg, func(t *testing.T) {
			res := MatchPackage(test.pkg, pluginv1.TargetMatcher_builder{CodegenPackage: htypes.Ptr(test.codegenPkg)}.Build())

			require.Equal(t, test.expected, res)
		})
		t.Run("MatchSpec "+test.pkg+" "+test.codegenPkg, func(t *testing.T) {
			res := MatchSpec(
				pluginv1.TargetSpec_builder{Ref: pluginv1.TargetRef_builder{Package: htypes.Ptr(test.pkg)}.Build()}.Build(),
				pluginv1.TargetMatcher_builder{CodegenPackage: htypes.Ptr(test.codegenPkg)}.Build(),
			)

			require.Equal(t, test.expected, res)
		})
	}
}

func TestMatchCodegenPackageDef(t *testing.T) {
	tests := []struct {
		pkg        string
		codegenPkg string
		outPath    string
		expected   Result
	}{
		{"foo/bar", "foo/bar", "file.txt", MatchYes},
		{"foo/bar", "foo/bar", "./", MatchYes},
		{"foo/bar", "foo/bar", "*.txt", MatchYes},
		{"foo/bar", "foo/bar", "some/*.txt", MatchNo},
		{"foo/bar", "foo/bar", "some/*/file.txt", MatchYes},
		{"foo", "foo/bar", "file.txt", MatchNo},
		{"foo", "foo/bar", "bar/file.txt", MatchYes},
		{"foo", "foo/bar", "bar/", MatchYes},
		{"", "foo/bar", "file.txt", MatchNo},
		{"", "foo/bar", "foo/bar/file.txt", MatchYes},
		{"foo/bar/baz", "foo/bar", "file.txt", MatchNo},
		{"unrelated/bar", "foo/bar", "file.txt", MatchNo},
	}
	for _, test := range tests {
		t.Run("MatchSpec "+test.pkg+" "+test.codegenPkg+" "+test.outPath, func(t *testing.T) {
			ref := tref.New(test.pkg, "foo", nil)

			outPath := pluginv1.TargetDef_Path_builder{
				FilePath:    nil,
				DirPath:     nil,
				Glob:        nil,
				CodegenTree: htypes.Ptr(pluginv1.TargetDef_Path_CODEGEN_MODE_COPY),
			}
			if hfs.IsGlob(test.outPath) {
				outPath.Glob = htypes.Ptr(test.outPath)
			} else if strings.HasSuffix(test.outPath, "/") {
				outPath.DirPath = htypes.Ptr(test.outPath)
			} else {
				outPath.FilePath = htypes.Ptr(test.outPath)
			}

			res := MatchDef(
				pluginv1.TargetSpec_builder{Ref: ref}.Build(),
				pluginv1.TargetDef_builder{
					Ref: ref,
					Outputs: []*pluginv1.TargetDef_Output{
						pluginv1.TargetDef_Output_builder{
							Paths: []*pluginv1.TargetDef_Path{
								outPath.Build(),
							},
						}.Build(),
					},
				}.Build(),
				pluginv1.TargetMatcher_builder{CodegenPackage: htypes.Ptr(test.codegenPkg)}.Build(),
			)

			require.Equal(t, test.expected.String(), res.String())
		})
	}
}

func TestPackages(t *testing.T) {
	dir := t.TempDir()
	require.NoError(t, os.MkdirAll(filepath.Join(dir, "foo/bar/baz"), os.ModePerm))
	require.NoError(t, os.MkdirAll(filepath.Join(dir, "hello/world"), os.ModePerm))

	var pkgs []string
	for pkg, err := range Packages(t.Context(), OSPackageProvider(dir, nil), pluginv1.TargetMatcher_builder{PackagePrefix: htypes.Ptr("")}.Build()) {
		require.NoError(t, err)

		pkgs = append(pkgs, pkg)
	}

	assert.Equal(t, []string{"", "foo", "foo/bar", "foo/bar/baz", "hello", "hello/world"}, pkgs)
}

func TestPackagesSubselection(t *testing.T) {
	dir := t.TempDir()
	require.NoError(t, os.MkdirAll(filepath.Join(dir, "foo/bar/baz"), os.ModePerm))
	require.NoError(t, os.MkdirAll(filepath.Join(dir, "hello/world"), os.ModePerm))

	var pkgs []string
	for pkg, err := range Packages(t.Context(), OSPackageProvider(dir, nil), pluginv1.TargetMatcher_builder{PackagePrefix: htypes.Ptr("foo/bar")}.Build()) {
		require.NoError(t, err)

		pkgs = append(pkgs, pkg)
	}

	assert.Equal(t, []string{"foo/bar", "foo/bar/baz"}, pkgs)
}

func TestLongestCommonPath(t *testing.T) {
	tests := []struct {
		name         string
		paths        []string
		expectedPath string
		expectedOk   bool
	}{
		{
			name:         "empty slice",
			paths:        []string{},
			expectedPath: "",
			expectedOk:   false,
		},
		{
			name:         "single path",
			paths:        []string{"foo/bar/baz"},
			expectedPath: "foo/bar/baz",
			expectedOk:   true,
		},
		{
			name:         "two identical paths",
			paths:        []string{"foo/bar", "foo/bar"},
			expectedPath: "foo/bar",
			expectedOk:   true,
		},
		{
			name:         "common prefix",
			paths:        []string{"foo/bar/baz", "foo/bar/qux"},
			expectedPath: "foo/bar",
			expectedOk:   true,
		},
		{
			name:         "multiple paths with common prefix",
			paths:        []string{"foo/bar/baz", "foo/bar/qux", "foo/baz"},
			expectedPath: "foo",
			expectedOk:   true,
		},
		{
			name:         "no common prefix",
			paths:        []string{"foo/bar", "hello/world"},
			expectedPath: "",
			expectedOk:   true,
		},
		{
			name:         "segment boundary - bin vs binder",
			paths:        []string{"usr/bin", "usr/binder"},
			expectedPath: "usr",
			expectedOk:   true,
		},
		{
			name:         "segment boundary - common word prefix",
			paths:        []string{"package/bar", "packages/baz"},
			expectedPath: "",
			expectedOk:   true,
		},
		{
			name:         "nested paths",
			paths:        []string{"a/b/c/d", "a/b/c", "a/b"},
			expectedPath: "a/b",
			expectedOk:   true,
		},
		{
			name:         "root level paths",
			paths:        []string{"a", "b", "c"},
			expectedPath: "",
			expectedOk:   true,
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			path, ok := longestCommonPath(test.paths)

			assert.Equal(t, test.expectedOk, ok)
			assert.Equal(t, test.expectedPath, path)
		})
	}
}

func testMatch(m *pluginv1.TargetMatcher) Result {
	switch m.WhichItem() {
	case pluginv1.TargetMatcher_Label_case:
		switch m.GetLabel() {
		case "yes":
			return MatchYes
		case "no":
			return MatchNo
		case "shrug":
			return MatchShrug
		default:
			panic("unhandled")
		}
	case pluginv1.TargetMatcher_Not_case:
		return runNot(m, testMatch)
	case pluginv1.TargetMatcher_Or_case:
		return runOr(m, testMatch)
	case pluginv1.TargetMatcher_And_case:
		return runAnd(m, testMatch)
	default:
		panic("unhandled")
	}
}

func TestNot(t *testing.T) {
	tests := []struct {
		in       string
		expected Result
	}{
		{"yes", MatchNo},
		{"no", MatchYes},
		{"shrug", MatchShrug},
	}
	for _, test := range tests {
		t.Run(test.in, func(t *testing.T) {
			res := testMatch(Not(Label(test.in)))

			require.Equal(t, test.expected.String(), res.String())
		})
	}
}

func TestOr(t *testing.T) {
	tests := []struct {
		values   []string
		expected Result
	}{
		{[]string{}, MatchNo},
		{[]string{"yes"}, MatchYes},
		{[]string{"no"}, MatchNo},
		{[]string{"yes", "no"}, MatchYes},
		{[]string{"yes", "shrug"}, MatchYes},
		{[]string{"no", "shrug"}, MatchShrug},
		{[]string{"yes", "no", "shrug"}, MatchYes},
	}
	for _, test := range tests {
		for values := range hslices.Permute(test.values) {
			t.Run(fmt.Sprintf("%v", values), func(t *testing.T) {
				var items []*pluginv1.TargetMatcher
				for _, value := range values {
					items = append(items, Label(value))
				}

				// have to manually construct to bypass the auto inlining
				m := pluginv1.TargetMatcher_builder{Or: pluginv1.TargetMatchers_builder{Items: items}.Build()}.Build()

				res := testMatch(m)

				assert.Equal(t, test.expected.String(), res.String())

				expectedNot := runNot(nil, func(m *pluginv1.TargetMatcher) Result {
					return test.expected
				})

				t.Run("not factorized", func(t *testing.T) {
					res := testMatch(Not(m))

					assert.Equal(t, expectedNot.String(), res.String())
				})

				t.Run("not distributed", func(t *testing.T) {
					var items []*pluginv1.TargetMatcher
					for _, value := range values {
						items = append(items, Not(Label(value)))
					}
					m := pluginv1.TargetMatcher_builder{And: pluginv1.TargetMatchers_builder{Items: items}.Build()}.Build()

					res := testMatch(m)

					assert.Equal(t, expectedNot.String(), res.String())
				})
			})
		}
	}
}

func TestAnd(t *testing.T) {
	tests := []struct {
		values   []string
		expected Result
	}{
		{[]string{}, MatchYes},
		{[]string{"yes"}, MatchYes},
		{[]string{"no"}, MatchNo},
		{[]string{"yes", "no"}, MatchNo},
		{[]string{"yes", "shrug"}, MatchShrug},
		{[]string{"no", "shrug"}, MatchNo},
		{[]string{"yes", "no", "shrug"}, MatchNo},
	}
	for _, test := range tests {
		for values := range hslices.Permute(test.values) {
			t.Run(fmt.Sprintf("%v", values), func(t *testing.T) {
				var items []*pluginv1.TargetMatcher
				for _, value := range values {
					items = append(items, Label(value))
				}

				// have to manually construct to bypass the auto inlining
				m := pluginv1.TargetMatcher_builder{And: pluginv1.TargetMatchers_builder{Items: items}.Build()}.Build()

				res := testMatch(m)

				assert.Equal(t, test.expected.String(), res.String())

				expectedNot := runNot(nil, func(m *pluginv1.TargetMatcher) Result {
					return test.expected
				})

				t.Run("not factorized", func(t *testing.T) {
					res := testMatch(Not(m))

					assert.Equal(t, expectedNot.String(), res.String())
				})

				t.Run("not distributed", func(t *testing.T) {
					var items []*pluginv1.TargetMatcher
					for _, value := range values {
						items = append(items, Not(Label(value)))
					}
					m := pluginv1.TargetMatcher_builder{Or: pluginv1.TargetMatchers_builder{Items: items}.Build()}.Build()

					res := testMatch(m)

					assert.Equal(t, expectedNot.String(), res.String())
				})
			})
		}
	}
}
