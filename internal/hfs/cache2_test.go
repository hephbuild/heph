package hfs

import (
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"sync/atomic"
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

// buildTree creates a small directory tree under root for use in tests:
//
//	root/
//	  a/
//	    a1.txt  ("hello a1")
//	    a2.txt  ("hello a2")
//	  b/
//	    b1.txt  ("hello b1")
//	  c.txt     ("hello c")
func buildTree(t *testing.T, root string) {
	t.Helper()

	for _, d := range []string{
		filepath.Join(root, "a"),
		filepath.Join(root, "b"),
	} {
		require.NoError(t, os.MkdirAll(d, 0755))
	}

	for path, content := range map[string]string{
		filepath.Join(root, "a", "a1.txt"): "hello a1",
		filepath.Join(root, "a", "a2.txt"): "hello a2",
		filepath.Join(root, "b", "b1.txt"): "hello b1",
		filepath.Join(root, "c.txt"):       "hello c",
	} {
		require.NoError(t, os.WriteFile(path, []byte(content), 0644))
	}
}

// collectWalk walks from root (absolute) and returns visited absolute paths.
func collectWalk(t *testing.T, cache *FSCache, root string) []string {
	t.Helper()

	var paths []string
	err := fs.WalkDir(cache, root, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}

		paths = append(paths, path)
		return nil
	})
	assert.NoError(t, err)

	return paths
}

// ---------------------------------------------------------------------------
// Basic correctness
// ---------------------------------------------------------------------------

func TestFSCache2_WalkAll(t *testing.T) {
	root := t.TempDir()
	buildTree(t, root)

	cache := NewFSCache()
	paths := collectWalk(t, cache, root)

	assert.Equal(t, []string{
		root,
		filepath.Join(root, "a"),
		filepath.Join(root, "a", "a1.txt"),
		filepath.Join(root, "a", "a2.txt"),
		filepath.Join(root, "b"),
		filepath.Join(root, "b", "b1.txt"),
		filepath.Join(root, "c.txt"),
	}, paths)
}

func TestFSCache2_WalkSubdir(t *testing.T) {
	root := t.TempDir()
	buildTree(t, root)

	cache := NewFSCache()
	subdir := filepath.Join(root, "a")
	paths := collectWalk(t, cache, subdir)

	assert.Equal(t, []string{
		subdir,
		filepath.Join(subdir, "a1.txt"),
		filepath.Join(subdir, "a2.txt"),
	}, paths)
}

// ---------------------------------------------------------------------------
// Caching: directory only read once
// ---------------------------------------------------------------------------

func TestFSCache2_DirectoryCachedAfterFirstWalk(t *testing.T) {
	root := t.TempDir()
	buildTree(t, root)

	cache := NewFSCache()
	first := collectWalk(t, cache, root)

	// File added after the cache is warm — second walk must not see it.
	require.NoError(t, os.WriteFile(filepath.Join(root, "new.txt"), []byte("new"), 0644))

	second := collectWalk(t, cache, root)
	assert.Equal(t, first, second, "second walk should return cached results")
}

// ---------------------------------------------------------------------------
// Shared cache across separate walks of different subtrees
// ---------------------------------------------------------------------------

func TestFSCache2_SharedCacheAcrossRoots(t *testing.T) {
	root := t.TempDir()
	buildTree(t, root)

	cache := NewFSCache()

	// First walk populates the cache.
	_ = collectWalk(t, cache, root)

	// Add a file — the cache is warm so a walk starting inside "a" should
	// not see the new top-level file.
	require.NoError(t, os.WriteFile(filepath.Join(root, "new.txt"), []byte("new"), 0644))

	subPaths := collectWalk(t, cache, filepath.Join(root, "a"))
	assert.Equal(t, []string{
		filepath.Join(root, "a"),
		filepath.Join(root, "a", "a1.txt"),
		filepath.Join(root, "a", "a2.txt"),
	}, subPaths)
}

// ---------------------------------------------------------------------------
// SkipDir
// ---------------------------------------------------------------------------

func TestFSCache2_SkipDir(t *testing.T) {
	root := t.TempDir()
	buildTree(t, root)

	cache := NewFSCache()
	dirA := filepath.Join(root, "a")

	var paths []string
	err := fs.WalkDir(cache, root, func(path string, d fs.DirEntry, err error) error {
		require.NoError(t, err)
		paths = append(paths, path)
		if path == dirA {
			return fs.SkipDir
		}
		return nil
	})
	require.NoError(t, err)

	assert.Equal(t, []string{
		root,
		dirA, // visited but children skipped
		filepath.Join(root, "b"),
		filepath.Join(root, "b", "b1.txt"),
		filepath.Join(root, "c.txt"),
	}, paths)
}

// ---------------------------------------------------------------------------
// Open + Stat
// ---------------------------------------------------------------------------

func TestFSCache2_StatRoot(t *testing.T) {
	root := t.TempDir()
	buildTree(t, root)

	cache := NewFSCache()

	f, err := cache.Open(root)
	require.NoError(t, err)
	defer f.Close()

	info, err := f.Stat()
	require.NoError(t, err)
	assert.True(t, info.IsDir())
}

func TestFSCache2_StatFile(t *testing.T) {
	root := t.TempDir()
	buildTree(t, root)

	cache := NewFSCache()
	path := filepath.Join(root, "c.txt")

	f, err := cache.Open(path)
	require.NoError(t, err)
	defer f.Close()

	info, err := f.Stat()
	require.NoError(t, err)
	assert.False(t, info.IsDir())
	assert.Equal(t, "c.txt", info.Name())
}

// ---------------------------------------------------------------------------
// Read file contents
// ---------------------------------------------------------------------------

func TestFSCache2_ReadFile(t *testing.T) {
	root := t.TempDir()
	buildTree(t, root)

	cache := NewFSCache()

	f, err := cache.Open(filepath.Join(root, "a", "a1.txt"))
	require.NoError(t, err)
	defer f.Close()

	data, err := io.ReadAll(f)
	require.NoError(t, err)
	assert.Equal(t, "hello a1", string(data))
}

func TestFSCache2_ReadFileAfterWalk(t *testing.T) {
	root := t.TempDir()
	buildTree(t, root)

	cache := NewFSCache()
	collectWalk(t, cache, root) // warm the cache

	f, err := cache.Open(filepath.Join(root, "b", "b1.txt"))
	require.NoError(t, err)
	defer f.Close()

	data, err := io.ReadAll(f)
	require.NoError(t, err)
	assert.Equal(t, "hello b1", string(data))
}

// ---------------------------------------------------------------------------
// Non-existent paths
// ---------------------------------------------------------------------------

func TestFSCache2_OpenNonExistent(t *testing.T) {
	cache := NewFSCache()

	_, err := cache.Open("/this/path/does/not/exist")
	require.Error(t, err)
	assert.ErrorIs(t, err, fs.ErrNotExist)
}

func TestFSCache2_WalkNonExistentRoot(t *testing.T) {
	cache := NewFSCache()

	err := fs.WalkDir(cache, "/this/path/does/not/exist", func(path string, d fs.DirEntry, err error) error {
		return err
	})
	require.Error(t, err)
}

// ---------------------------------------------------------------------------
// ReadDir on a directory file
// ---------------------------------------------------------------------------

func TestFSCache2_ReadDirFile(t *testing.T) {
	root := t.TempDir()
	buildTree(t, root)

	cache := NewFSCache()

	f, err := cache.Open(filepath.Join(root, "a"))
	require.NoError(t, err)
	defer f.Close()

	rdf, ok := f.(fs.ReadDirFile)
	require.True(t, ok, "directory file must implement fs.ReadDirFile")

	entries, err := rdf.ReadDir(-1)
	require.NoError(t, err)

	names := make([]string, len(entries))
	for i, e := range entries {
		names[i] = e.Name()
	}
	assert.Equal(t, []string{"a1.txt", "a2.txt"}, names)
}

func TestFSCache2_ReadDirFilePaged(t *testing.T) {
	root := t.TempDir()
	buildTree(t, root)

	cache := NewFSCache()

	f, err := cache.Open(filepath.Join(root, "a"))
	require.NoError(t, err)
	defer f.Close()

	rdf := f.(fs.ReadDirFile)

	e1, err := rdf.ReadDir(1)
	require.NoError(t, err)
	require.Len(t, e1, 1)
	assert.Equal(t, "a1.txt", e1[0].Name())

	e2, err := rdf.ReadDir(1)
	if err != nil {
		assert.Equal(t, io.EOF, err)
	}
	require.Len(t, e2, 1)
	assert.Equal(t, "a2.txt", e2[0].Name())

	// Exhausted — must return io.EOF with empty slice.
	e3, err := rdf.ReadDir(1)
	assert.Equal(t, io.EOF, err)
	assert.Empty(t, e3)
}

// ---------------------------------------------------------------------------
// Read on a directory returns an error
// ---------------------------------------------------------------------------

func TestFSCache2_ReadOnDirErrors(t *testing.T) {
	root := t.TempDir()
	cache := NewFSCache()

	f, err := cache.Open(root)
	require.NoError(t, err)
	defer f.Close()

	_, err = f.Read(make([]byte, 8))
	require.Error(t, err)
}

// ---------------------------------------------------------------------------
// Concurrent walks are safe
// ---------------------------------------------------------------------------

func TestFSCache2_ConcurrentWalks(t *testing.T) {
	root := t.TempDir()
	buildTree(t, root)

	cache := NewFSCache()
	const goroutines = 8
	const wantPaths = 7 // root + 2 dirs + 4 files

	var ok atomic.Int32
	done := make(chan struct{}, goroutines)

	for range goroutines {
		go func() {
			defer func() { done <- struct{}{} }()
			paths := collectWalk(t, cache, root)
			if len(paths) == wantPaths {
				ok.Add(1)
			}
		}()
	}
	for range goroutines {
		<-done
	}

	assert.Equal(t, int32(goroutines), ok.Load())
}
