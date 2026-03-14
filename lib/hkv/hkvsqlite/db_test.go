package hkvsqlite

import (
	"context"
	"io"
	"path/filepath"
	"testing"
	"time"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestKV_CRUD(t *testing.T) {
	ctx := context.Background()
	tmpDir := t.TempDir()

	dbPath := filepath.Join(tmpDir, "test.db")
	kv := New(dbPath)
	defer kv.Close()

	key := "test-key"
	value := []byte("test-value")
	metadata := map[string]string{"foo": "bar", "hello": "world"}

	// Test Exists (false)
	exists, err := kv.Exists(ctx, key)
	require.NoError(t, err)
	assert.False(t, exists)

	// Test Set
	err = kv.Set(ctx, key, value, metadata, 0)
	require.NoError(t, err)

	// Test Exists (true)
	exists, err = kv.Exists(ctx, key)
	require.NoError(t, err)
	assert.True(t, exists)

	// Test Get
	gotValue, gotMetadata, found, err := kv.Get(ctx, key)
	require.NoError(t, err)
	assert.True(t, found)
	assert.Equal(t, value, gotValue)
	assert.Equal(t, metadata, gotMetadata)

	// Test Delete
	err = kv.Delete(ctx, key)
	require.NoError(t, err)

	// Test Exists (false) after delete
	exists, err = kv.Exists(ctx, key)
	require.NoError(t, err)
	assert.False(t, exists)

	// Test Get (not found)
	_, _, found, err = kv.Get(ctx, key)
	require.NoError(t, err)
	assert.False(t, found)
}

func TestKV_IO(t *testing.T) {
	ctx := context.Background()
	tmpDir := t.TempDir()

	dbPath := filepath.Join(tmpDir, "test-io.db")
	kv := New(dbPath)
	defer kv.Close()

	key := "io-key"
	value := []byte("io-test-value")
	metadata := map[string]string{"io": "true"}

	// Test Writer
	w, err := kv.Writer(ctx, key, metadata, 0)
	require.NoError(t, err)
	n, err := w.Write(value)
	require.NoError(t, err)
	assert.Equal(t, len(value), n)
	err = w.Close()
	require.NoError(t, err)

	// Test Reader
	r, found, err := kv.Reader(ctx, key)
	require.NoError(t, err)
	assert.True(t, found)
	gotMeta, ok, err := kv.GetMeta(ctx, key)
	require.NoError(t, err)
	assert.True(t, ok)
	assert.Equal(t, metadata, gotMeta)
	gotValue, err := io.ReadAll(r)
	require.NoError(t, err)
	assert.Equal(t, value, gotValue)
	err = r.Close()
	require.NoError(t, err)
}

func TestKV_List(t *testing.T) {
	ctx := context.Background()
	tmpDir := t.TempDir()

	dbPath := filepath.Join(tmpDir, "test-list.db")
	kv := New(dbPath)
	defer kv.Close()

	entries := []struct {
		key      string
		value    []byte
		metadata map[string]string
	}{
		{"k1", []byte("v1"), map[string]string{"type": "a", "env": "prod"}},
		{"k2", []byte("v2"), map[string]string{"type": "b", "env": "prod"}},
		{"k3", []byte("v3"), map[string]string{"type": "a", "env": "dev"}},
	}

	for _, e := range entries {
		err := kv.Set(ctx, e.key, e.value, e.metadata, 0)
		require.NoError(t, err)
	}

	// List all
	var allKeys []string
	for k, err := range kv.ListKeys(ctx, nil) {
		require.NoError(t, err)
		allKeys = append(allKeys, k)
	}
	assert.ElementsMatch(t, []string{"k1", "k2", "k3"}, allKeys)

	// List with filter type=a
	var typeAKeys []string
	for k, err := range kv.ListKeys(ctx, map[string]string{"type": "a"}) {
		require.NoError(t, err)
		typeAKeys = append(typeAKeys, k)
	}
	assert.ElementsMatch(t, []string{"k1", "k3"}, typeAKeys)

	// List with filter env=prod
	var prodKeys []string
	for k, err := range kv.ListKeys(ctx, map[string]string{"env": "prod"}) {
		require.NoError(t, err)
		prodKeys = append(prodKeys, k)
	}
	assert.ElementsMatch(t, []string{"k1", "k2"}, prodKeys)

	// List with both filters
	var prodTypeAKeys []string
	for k, err := range kv.ListKeys(ctx, map[string]string{"type": "a", "env": "prod"}) {
		require.NoError(t, err)
		prodTypeAKeys = append(prodTypeAKeys, k)
	}
	assert.ElementsMatch(t, []string{"k1"}, prodTypeAKeys)
}

func TestKV_TTL_Unsupported(t *testing.T) {
	ctx := context.Background()
	tmpDir := t.TempDir()

	dbPath := filepath.Join(tmpDir, "test-ttl.db")
	kv := New(dbPath)
	defer kv.Close()

	err := kv.Set(ctx, "k", []byte("v"), nil, time.Hour)
	require.ErrorContains(t, err, "unsupported")

	_, err = kv.Writer(ctx, "k", nil, time.Hour)
	require.ErrorContains(t, err, "unsupported")
}

func TestKV_Metadata(t *testing.T) {
	ctx := context.Background()
	tmpDir := t.TempDir()

	dbPath := filepath.Join(tmpDir, "test-metadata.db")
	kv := New(dbPath)
	defer kv.Close()

	key := "meta-key"
	value := []byte("v")

	// Set with no metadata
	err := kv.Set(ctx, key, value, nil, 0)
	require.NoError(t, err)

	_, gotMeta, _, err := kv.Get(ctx, key)
	require.NoError(t, err)
	assert.Empty(t, gotMeta)

	// Set with empty metadata
	err = kv.Set(ctx, key, value, map[string]string{}, 0)
	require.NoError(t, err)

	_, gotMeta, _, err = kv.Get(ctx, key)
	require.NoError(t, err)
	assert.Empty(t, gotMeta)

	// Update with metadata
	meta := map[string]string{"a": "1"}
	err = kv.Set(ctx, key, value, meta, 0)
	require.NoError(t, err)

	_, gotMeta, _, err = kv.Get(ctx, key)
	require.NoError(t, err)
	assert.Equal(t, meta, gotMeta)

	// Update with different metadata (should replace)
	meta2 := map[string]string{"b": "2"}
	err = kv.Set(ctx, key, value, meta2, 0)
	require.NoError(t, err)

	_, gotMeta, _, err = kv.Get(ctx, key)
	require.NoError(t, err)
	assert.Equal(t, meta2, gotMeta)
}
