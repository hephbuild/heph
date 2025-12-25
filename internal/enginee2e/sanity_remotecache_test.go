package enginee2e

import (
	"bytes"
	"context"
	"io"
	"testing"

	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hlog/hlogtest"
	"github.com/hephbuild/heph/internal/htypes"

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
	ctx = hlog.ContextWithLogger(ctx, hlogtest.NewLogger(t))

	cache := &mockCache{}

	pkg := t.Name()

	staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("t1"),
				}.Build(),
				Driver: htypes.Ptr("sh"),
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{`echo hello > $OUT`}),
					"out": hstructpb.NewStringsValue([]string{"out"}),
				},
			}.Build(),
		},
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("t2"),
				}.Build(),
				Driver: htypes.Ptr("sh"),
				Config: map[string]*structpb.Value{
					"run":  hstructpb.NewStringsValue([]string{`echo world > $OUT`}),
					"out":  hstructpb.NewStringsValue([]string{"out"}),
					"deps": hstructpb.NewStringsValue([]string{"//" + pkg + ":t1"}),
				},
			}.Build(),
		},
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("t3"),
				}.Build(),
				Driver: htypes.Ptr("sh"),
				Config: map[string]*structpb.Value{
					"run":  hstructpb.NewStringsValue([]string{`echo world > $OUT`}),
					"out":  hstructpb.NewStringsValue([]string{"out"}),
					"deps": hstructpb.NewStringsValue([]string{"//" + pkg + ":t1", "//" + pkg + ":t2"}),
				},
			}.Build(),
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

			manifest, err := e.ResultMetaFromRef(ctx, rs, tref.New(pkg, "t1", nil), []string{""})
			require.NoError(t, err)

			assert.Equal(t, "e52c3f7fe43c3c02", manifest.Hashin)
			assert.Equal(t, "d4fd9c2c4c50146f", manifest.Artifacts[0].Hashout)
		}

		{
			rs, clean := e.NewRequestState()
			defer clean()

			res, err := e.Result(ctx, rs, pkg, "t2", []string{""})
			require.NoError(t, err)
			defer res.Unlock(ctx)

			require.Len(t, res.Artifacts, 1)

			manifest, err := e.ResultMetaFromRef(ctx, rs, tref.New(pkg, "t2", nil), []string{""})
			require.NoError(t, err)

			assert.Equal(t, "7103b83d26040e7a", manifest.Hashin)
			assert.Equal(t, "3b0f519635c52211", manifest.Artifacts[0].Hashout)
		}

		{
			rs, clean := e.NewRequestState()
			defer clean()

			res, err := e.Result(ctx, rs, pkg, "t3", []string{""})
			require.NoError(t, err)
			defer res.Unlock(ctx)

			require.Len(t, res.Artifacts, 1)

			manifest, err := e.ResultMetaFromRef(ctx, rs, tref.New(pkg, "t3", nil), []string{""})
			require.NoError(t, err)

			assert.Equal(t, "7a65fe455c8b6178", manifest.Hashin)
			assert.Equal(t, "3b0f519635c52211", manifest.Artifacts[0].Hashout)
		}

		assert.Len(t, cache.storeWrites, 6)
		for k, c := range cache.storeWrites {
			assert.Equalf(t, 1, c, "cache hit count for %v", k)
		}
	}
}
