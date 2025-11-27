package tmatch

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/hephbuild/heph/internal/htypes"
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

func TestMatchPackageCodegen(t *testing.T) {
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
