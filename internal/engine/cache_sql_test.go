package engine_test

import (
	"context"
	"io"
	"path/filepath"
	"testing"

	"github.com/hephbuild/heph/internal/engine"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/stretchr/testify/require"
)

func TestSQLCache(t *testing.T) {
	tempDir := t.TempDir()
	dbPath := filepath.Join(tempDir, "cache.db")
	db, err := engine.OpenSQLCacheDB(dbPath)
	require.NoError(t, err)
	defer db.Close()

	cache := engine.NewSQLCache(db)

	ctx := context.Background()
	pkg := "pkg"
	name := "target"
	ref := (&pluginv1.TargetRef_builder{
		Package: &pkg,
		Name:    &name,
	}).Build()
	hashin := "hash123"

	// Test Write & Read
	w, err := cache.Writer(ctx, ref, hashin, "art1")
	require.NoError(t, err)
	_, err = io.WriteString(w, "hello world")
	require.NoError(t, err)
	require.NoError(t, w.Close())

	exists, err := cache.Exists(ctx, ref, hashin, "art1")
	require.NoError(t, err)
	require.True(t, exists)
	r, err := cache.Reader(ctx, ref, hashin, "art1")
	require.NoError(t, err)
	b, err := io.ReadAll(r)
	require.NoError(t, err)
	require.NoError(t, r.Close())
	require.Equal(t, "hello world", string(b))

	// Test Read Not Exist
	exists, err = cache.Exists(ctx, ref, hashin, "art2")
	require.NoError(t, err)
	require.False(t, exists)
	_, err = cache.Reader(ctx, ref, hashin, "art2")
	require.ErrorIs(t, err, engine.LocalCacheNotFoundError)

	// Test ListArtifacts
	w, err = cache.Writer(ctx, ref, hashin, "art2")
	require.NoError(t, err)
	require.NoError(t, w.Close())

	artSeq := cache.ListArtifacts(ctx, ref, hashin, "")
	var artifacts []string
	for a, e := range artSeq {
		require.NoError(t, e)
		artifacts = append(artifacts, a)
	}
	require.ElementsMatch(t, []string{"art1", "art2"}, artifacts)

	// Test ListVersions
	w, err = cache.Writer(ctx, ref, "hash456", "art1")
	require.NoError(t, err)
	require.NoError(t, w.Close())

	verSeq := cache.ListVersions(ctx, ref, "")
	var versions []string
	for v, e := range verSeq {
		require.NoError(t, e)
		versions = append(versions, v)
	}
	require.ElementsMatch(t, []string{"hash123", "hash456"}, versions)
}
