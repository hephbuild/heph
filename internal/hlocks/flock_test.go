package hlocks

import (
	"testing"

	"github.com/hephbuild/heph/internal/hfs"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestFlockMultiRLock(t *testing.T) {
	ctx := t.Context()

	fs := newfs(t)
	m := NewFlock(hfs.At(fs, t.Name()+".lock"), t.Name())

	err := m.RLock(ctx)
	require.NoError(t, err)

	err = m.RLock(ctx)
	require.NoError(t, err)

	err = m.RUnlock()
	require.NoError(t, err)

	ok, err := m.TryLock(ctx)
	require.NoError(t, err)
	assert.False(t, ok)

	err = m.RUnlock()
	require.NoError(t, err)

	ok, err = m.TryLock(ctx)
	require.NoError(t, err)
	assert.True(t, ok)

	err = m.Unlock()
	require.NoError(t, err)
}
