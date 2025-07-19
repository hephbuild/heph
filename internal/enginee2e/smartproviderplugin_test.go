package enginee2e

import (
	"io"
	"testing"

	"github.com/hephbuild/heph/internal/enginee2e/pluginsmartprovidertest"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/plugin/pluginexec"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestSmartProviderPlugin(t *testing.T) {
	ctx := t.Context()

	dir := t.TempDir()

	e, err := engine.New(ctx, dir, engine.Config{})
	require.NoError(t, err)

	_, err = e.RegisterProvider(ctx, pluginsmartprovidertest.New())
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, pluginexec.New(), nil)
	require.NoError(t, err)
	_, err = e.RegisterDriver(ctx, pluginexec.NewBash(), nil)
	require.NoError(t, err)

	rs, clean := e.NewRequestState()
	defer clean()

	res, err := e.Result(ctx, rs, "", "do", []string{engine.AllOutputs})
	require.NoError(t, err)
	defer res.Unlock(ctx)

	require.Len(t, res.Artifacts, 1)

	r, err := hartifact.FileReader(ctx, res.FindOutputs("")[0].Artifact)
	require.NoError(t, err)
	defer r.Close()

	b, err := io.ReadAll(r)
	require.NoError(t, err)

	assert.Equal(t, "parent: hello\n\n", string(b))
}
