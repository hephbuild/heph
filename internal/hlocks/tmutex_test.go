package hlocks

import (
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"testing"
)

func TestTMutexRLock2Lock(t *testing.T) {
	ctx := t.Context()
	fs := newfs(t)

	m := NewT(
		NewFlock(fs, t.Name(), t.Name()+".outer.lock"),
		NewFlock(fs, t.Name(), t.Name()+".inner.lock"),
	)

	err := m.RLock(ctx)
	require.NoError(t, err)

	err = m.RLock2Lock(ctx)
	require.NoError(t, err)

	err = m.Unlock()
	require.NoError(t, err)
}

func TestTMutexLock2RLock(t *testing.T) {
	ctx := t.Context()
	fs := newfs(t)

	m := NewT(
		NewFlock(fs, t.Name(), t.Name()+".outer.lock"),
		NewFlock(fs, t.Name(), t.Name()+".inner.lock"),
	)

	err := m.Lock(ctx)
	require.NoError(t, err)

	err = m.Lock2RLock(ctx)
	require.NoError(t, err)

	err = m.RUnlock()
	require.NoError(t, err)
}

func TestTMutex(t *testing.T) {
	ctx := t.Context()

	//outer := NewMutex("outer")
	//inner := NewMutex("inner")
	//m1 := NewT(outer, inner)
	//m2 := NewT(outer, inner)

	fs := newfs(t)
	m1 := NewT(
		NewFlock(fs, t.Name(), t.Name()+".outer.lock"),
		NewFlock(fs, t.Name(), t.Name()+".inner.lock"),
	)
	m2 := NewT(
		NewFlock(fs, t.Name(), t.Name()+".outer.lock"),
		NewFlock(fs, t.Name(), t.Name()+".inner.lock"),
	)

	// try lock/rlock
	err := m1.Lock(ctx)
	require.NoError(t, err)

	ok, err := m1.TryRLock(ctx)
	require.NoError(t, err)
	assert.False(t, ok)

	// release, and try rlock
	err = m1.Unlock()
	require.NoError(t, err)

	err = m1.RLock(ctx)
	require.NoError(t, err)

	err = m2.RLock(ctx)
	require.NoError(t, err)

	// attempt to lock
	ok, err = m1.TryLock(ctx)
	require.NoError(t, err)
	assert.False(t, ok)

	//cleanup
	err = m1.RUnlock()
	require.NoError(t, err)

	// attempt to lock
	ok, err = m1.TryLock(ctx)
	require.NoError(t, err)
	assert.False(t, ok)

	// full cleanup
	err = m2.RUnlock()
	require.NoError(t, err)

	// lock 2 rlock
	err = m1.Lock(ctx)
	require.NoError(t, err)

	err = m1.Lock2RLock(ctx)
	require.NoError(t, err)

	ok, err = m1.TryLock(ctx)
	require.NoError(t, err)
	assert.False(t, ok)

	ok, err = m2.TryRLock(ctx)
	require.NoError(t, err)
	assert.True(t, ok)

	err = m2.RUnlock()
	require.NoError(t, err)

	// rlock 2 lock
	err = m1.RLock2Lock(ctx)
	require.NoError(t, err)

	ok, err = m1.TryLock(ctx)
	require.NoError(t, err)
	assert.False(t, ok)

	ok, err = m1.TryRLock(ctx)
	require.NoError(t, err)
	assert.False(t, ok)

	err = m1.Unlock()
	require.NoError(t, err)
}
