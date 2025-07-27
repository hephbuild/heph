package enginee2e

import (
	"bytes"
	"context"
	"github.com/hephbuild/heph/internal/htypes"
	"io"
	"testing"

	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/lib/pluginsdk"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"

	"github.com/hephbuild/heph/internal/engine"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/pluginexec"
	"github.com/hephbuild/heph/plugin/pluginstaticprovider"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"google.golang.org/protobuf/types/known/structpb"
)

type mockCache struct {
	store       map[string][]byte
	storeWrites map[string]int
}

func (m *mockCache) Store(ctx context.Context, key string, r io.Reader) error {
	b, err := io.ReadAll(r)
	if err != nil {
		return err
	}
	if m.store == nil {
		m.store = make(map[string][]byte)
	}

	m.store[key] = b

	if m.storeWrites == nil {
		m.storeWrites = make(map[string]int)
	}
	m.storeWrites[key]++

	return nil
}

func (m *mockCache) Get(ctx context.Context, key string) (io.ReadCloser, error) {
	b, ok := m.store[key]
	if !ok {
		return nil, pluginsdk.ErrCacheNotFound
	}

	return io.NopCloser(bytes.NewReader(b)), nil
}

func TestSanityRemoteCache(t *testing.T) {
	ctx := t.Context()
	cache := &mockCache{}

	pkg := t.Name()

	staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("t1"),
				},
				Driver: htypes.Ptr("sh"),
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{`echo hello > $OUT`}),
					"out": hstructpb.NewStringsValue([]string{"out"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("t2"),
				},
				Driver: htypes.Ptr("sh"),
				Config: map[string]*structpb.Value{
					"run":  hstructpb.NewStringsValue([]string{`echo world > $OUT`}),
					"out":  hstructpb.NewStringsValue([]string{"out"}),
					"deps": hstructpb.NewStringsValue([]string{"//" + pkg + ":t1"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("t3"),
				},
				Driver: htypes.Ptr("sh"),
				Config: map[string]*structpb.Value{
					"run":  hstructpb.NewStringsValue([]string{`echo world > $OUT`}),
					"out":  hstructpb.NewStringsValue([]string{"out"}),
					"deps": hstructpb.NewStringsValue([]string{"//" + pkg + ":t1", "//" + pkg + ":t2"}),
				},
			},
		},
	})

	// Simulates 2 independent runs, with the same cache
	for i := 1; i <= 2; i++ {
		t.Log("RUN", i)

		dir := t.TempDir()

		e, err := engine.New(ctx, dir, engine.Config{})
		require.NoError(t, err)

		_, err = e.RegisterProvider(ctx, staticprovider)
		require.NoError(t, err)

		_, err = e.RegisterDriver(ctx, pluginexec.NewSh(), nil)
		require.NoError(t, err)

		_, err = e.RegisterCache("test", cache, true, true)
		require.NoError(t, err)

		{
			rs, clean := e.NewRequestState()
			defer clean()

			res, err := e.Result(ctx, rs, pkg, "t1", []string{""})
			require.NoError(t, err)
			defer res.Unlock(ctx)

			require.Len(t, res.Artifacts, 1)

			manifest, err := e.ResultMetaFromRef(ctx, rs, &tref.Ref{Package: htypes.Ptr(pkg), Name: htypes.Ptr("t1")}, []string{""})
			require.NoError(t, err)

			assert.Equal(t, "ead6120968cc07c5", manifest.Hashin)
			assert.Equal(t, "f406eea1fde8ad3f", manifest.Artifacts[0].Hashout)
		}

		{
			rs, clean := e.NewRequestState()
			defer clean()

			res, err := e.Result(ctx, rs, pkg, "t2", []string{""})
			require.NoError(t, err)
			defer res.Unlock(ctx)

			require.Len(t, res.Artifacts, 1)

			manifest, err := e.ResultMetaFromRef(ctx, rs, &tref.Ref{Package: htypes.Ptr(pkg), Name: htypes.Ptr("t2")}, []string{""})
			require.NoError(t, err)

			assert.Equal(t, "bce04ee30b680dde", manifest.Hashin)
			assert.Equal(t, "80c1a1818bce4532", manifest.Artifacts[0].Hashout)
		}

		{
			rs, clean := e.NewRequestState()
			defer clean()

			res, err := e.Result(ctx, rs, pkg, "t3", []string{""})
			require.NoError(t, err)
			defer res.Unlock(ctx)

			require.Len(t, res.Artifacts, 1)

			manifest, err := e.ResultMetaFromRef(ctx, rs, &tref.Ref{Package: htypes.Ptr(pkg), Name: htypes.Ptr("t3")}, []string{""})
			require.NoError(t, err)

			assert.Equal(t, "da9694d6b4a6091f", manifest.Hashin)
			assert.Equal(t, "80c1a1818bce4532", manifest.Artifacts[0].Hashout)
		}

		assert.Len(t, cache.storeWrites, 6)
		for _, c := range cache.storeWrites {
			assert.Equal(t, 1, c)
		}
	}
}
