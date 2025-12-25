package hfs_test

import (
	"slices"
	"testing"

	"github.com/hephbuild/heph/internal/hfs"
	"github.com/stretchr/testify/assert"
)

func TestHasPathPrefix(t *testing.T) {
	tests := []struct {
		name   string
		parent string
		child  string
		want   bool
	}{
		{
			name:   "direct child",
			parent: "parent",
			child:  "parent/child",
			want:   true,
		},
		{
			name:   "nested child",
			parent: "parent",
			child:  "parent/child/grandchild",
			want:   true,
		},
		{
			name:   "same path",
			parent: "parent",
			child:  "parent",
			want:   true,
		},
		{
			name:   "not a child",
			parent: "parent",
			child:  "other",
			want:   false,
		},
		{
			name:   "sibling",
			parent: "parent",
			child:  "parent2/child",
			want:   false,
		},
		{
			name:   "prefix match but not child",
			parent: "parent",
			child:  "parent2",
			want:   false,
		},
		{
			name:   "empty parent",
			parent: "",
			child:  "child",
			want:   false,
		},
		{
			name:   "empty child",
			parent: "parent",
			child:  "",
			want:   false,
		},
		{
			name:   "both empty",
			parent: "",
			child:  "",
			want:   true,
		},
		{
			name:   "deep nested",
			parent: "a/b",
			child:  "a/b/c/d/e",
			want:   true,
		},
		{
			name:   "parent is longer",
			parent: "a/b/c",
			child:  "a/b",
			want:   false,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := hfs.HasPathPrefix(tt.child, tt.parent)
			assert.Equal(t, tt.want, got)
		})
	}
}

func TestParentPaths(t *testing.T) {
	tests := []struct {
		name string
		path string
		want []string
	}{
		{
			name: "simple path",
			path: "a/b/c",
			want: []string{"a/b/c", "a/b", "a", ""},
		},
		{
			name: "two levels",
			path: "a/b",
			want: []string{"a/b", "a", ""},
		},
		{
			name: "single level",
			path: "a",
			want: []string{"a", ""},
		},
		{
			name: "empty path",
			path: "",
			want: []string{""},
		},
		{
			name: "deep path",
			path: "a/b/c/d/e",
			want: []string{"a/b/c/d/e", "a/b/c/d", "a/b/c", "a/b", "a", ""},
		},
		{
			name: "path with trailing slash",
			path: "a/b/",
			want: []string{"a/b/", "a/b", "a", ""},
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			var got []string
			for p := range hfs.ParentPaths(tt.path) {
				got = append(got, p)
			}
			assert.Equal(t, tt.want, got)
		})
	}
}

func TestParentPathsEarlyBreak(t *testing.T) {
	path := "a/b/c/d/e"
	var got []string

	count := 0
	for p := range hfs.ParentPaths(path) {
		got = append(got, p)
		count++
		if count == 3 {
			break
		}
	}

	assert.Equal(t, []string{"a/b/c/d/e", "a/b/c/d", "a/b/c"}, got)
}

func TestParentPathsCollectAll(t *testing.T) {
	path := "parent/child/grandchild"

	parents := slices.Collect(hfs.ParentPaths(path))

	assert.Equal(t, []string{"parent/child/grandchild", "parent/child", "parent", ""}, parents)
}
