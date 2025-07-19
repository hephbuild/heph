package enginee2e

import (
	"bytes"
	"context"
	"io"
	"os"
	"path/filepath"
	"testing"

	"github.com/hephbuild/heph/lib/pluginsdk"
	"go.uber.org/mock/gomock"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hfs/hfstest"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/pluginexec"
	"github.com/hephbuild/heph/plugin/pluginstaticprovider"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"google.golang.org/protobuf/types/known/structpb"
)

func TestDepsCache(t *testing.T) {
	ctx := t.Context()

	dir := t.TempDir()

	e, err := engine.New(ctx, dir, engine.Config{})
	require.NoError(t, err)

	staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "child",
				},
				Driver: "bash",
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{`echo hello > $OUT`}),
					"out": hstructpb.NewStringsValue([]string{"out_child"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "parent",
				},
				Driver: "bash",
				Config: map[string]*structpb.Value{
					"run":  hstructpb.NewStringsValue([]string{`echo $(cat $SRC) > $OUT`}),
					"deps": hstructpb.NewStringsValue([]string{"//:child"}),
					"out":  hstructpb.NewStringsValue([]string{"out_parent"}),
				},
			},
		},
	})

	_, err = e.RegisterProvider(ctx, staticprovider)
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, pluginexec.New(), nil)
	require.NoError(t, err)
	_, err = e.RegisterDriver(ctx, pluginexec.NewSh(), nil)
	require.NoError(t, err)
	_, err = e.RegisterDriver(ctx, pluginexec.NewBash(), nil)
	require.NoError(t, err)

	assertOut := func(res *engine.ExecuteResultLocks) {
		fs := hfstest.New(t)
		err = hartifact.Unpack(ctx, res.FindOutputs("")[0].Artifact, fs)
		require.NoError(t, err)

		b, err := hfs.ReadFile(fs, "out_parent")
		require.NoError(t, err)

		assert.Equal(t, "hello\n", string(b))
	}

	{ // This will run all
		res, err := e.Result(ctx, &engine.RequestState{}, "", "parent", []string{engine.AllOutputs})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		assertOut(res)
		res.Unlock(ctx)
	}

	{ // this should reuse cache from deps
		res, err := e.Result(ctx, &engine.RequestState{}, "", "parent", []string{engine.AllOutputs})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		assertOut(res)
		res.Unlock(ctx)
	}
}

func TestDepsCache2(t *testing.T) {
	ctx := t.Context()
	c := gomock.NewController(t)

	dir := t.TempDir()

	e, err := engine.New(ctx, dir, engine.Config{})
	require.NoError(t, err)

	provider := pluginsdk.NewMockProvider(c)

	provider.EXPECT().
		Config(gomock.Any(), gomock.Any()).
		Return(&pluginv1.ProviderConfigResponse{Name: "test_provider"}, nil).AnyTimes()

	provider.EXPECT().
		Get(gomock.Any(), gomock.Any()).
		DoAndReturn(func(ctx context.Context, request *pluginv1.GetRequest) (*pluginv1.GetResponse, error) {
			return &pluginv1.GetResponse{
				Spec: &pluginv1.TargetSpec{
					Ref:    &pluginv1.TargetRef{Package: "", Name: "child"},
					Driver: "test_driver",
				},
			}, nil
		}).Times(1)

	_, err = e.RegisterProvider(ctx, provider)
	require.NoError(t, err)

	driver := pluginsdk.NewMockDriver(c)

	driver.EXPECT().
		Config(gomock.Any(), gomock.Any()).
		Return(&pluginv1.ConfigResponse{Name: "test_driver"}, nil).AnyTimes()

	driver.EXPECT().
		Parse(gomock.Any(), gomock.Any()).
		Return(&pluginv1.ParseResponse{
			Target: &pluginv1.TargetDef{
				Ref:     &pluginv1.TargetRef{Package: "", Name: "child"},
				Inputs:  nil,
				Outputs: []string{""},
				Cache:   true,
			},
		}, nil).Times(1)

	driver.EXPECT().
		Run(gomock.Any(), gomock.Any()).
		DoAndReturn(func(ctx context.Context, request *pluginv1.RunRequest) (*pluginv1.RunResponse, error) {
			return &pluginv1.RunResponse{
				Artifacts: []*pluginv1.Artifact{
					{
						Group: "",
						Name:  "out",
						Type:  pluginv1.Artifact_TYPE_OUTPUT,
						Content: &pluginv1.Artifact_Raw{
							Raw: &pluginv1.Artifact_ContentRaw{
								Data: []byte("hello"),
								Path: "out.txt",
							},
						},
					},
				},
			}, nil
		}).Times(1)

	_, err = e.RegisterDriver(ctx, driver, nil)
	require.NoError(t, err)

	cache := pluginsdk.NewMockCache(c)

	cache.EXPECT().
		Get(gomock.Any(), "__child/a758f2d4b1f498ef/manifest.v1.json").
		Return(nil, pluginsdk.ErrNotFound).Times(1)

	for _, key := range []string{"__child/a758f2d4b1f498ef/manifest.v1.json", "__child/a758f2d4b1f498ef/out_out.tar"} {
		cache.EXPECT().
			Store(gomock.Any(), key, gomock.Any()).
			DoAndReturn(func(ctx context.Context, key string, reader io.Reader) error {
				b, err := io.ReadAll(reader)
				if err != nil {
					return err
				}

				cache.EXPECT().
					Get(gomock.Any(), key).
					Return(io.NopCloser(bytes.NewReader(b)), nil).Times(1)

				return nil
			}).Times(1)
	}

	_, err = e.RegisterCache("test", cache, true, true)
	require.NoError(t, err)

	assertOut := func(res *engine.ExecuteResultLocks) {
		fs := hfstest.New(t)
		err = hartifact.Unpack(ctx, res.FindOutputs("")[0].Artifact, fs)
		require.NoError(t, err)

		b, err := hfs.ReadFile(fs, "out.txt")
		require.NoError(t, err)

		assert.Equal(t, "hello", string(b))
	}

	{ // This will run all
		res, err := e.Result(ctx, &engine.RequestState{}, "", "child", []string{engine.AllOutputs})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		assertOut(res)
		res.Unlock(ctx)
	}

	{ // this should reuse cache from deps
		res, err := e.Result(ctx, &engine.RequestState{}, "", "child", []string{engine.AllOutputs})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		assertOut(res)
		res.Unlock(ctx)
	}

	err = os.RemoveAll(filepath.Join(dir, ".heph"))
	require.NoError(t, err)

	{ // this should reuse cache from deps
		res, err := e.Result(ctx, &engine.RequestState{}, "", "child", []string{engine.AllOutputs})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		assertOut(res)
		res.Unlock(ctx)
	}
}
