package enginee2e

import (
	"bytes"
	"context"
	"io"
	"os"
	"path/filepath"
	"testing"

	"github.com/hephbuild/heph/internal/htypes"

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
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(""),
					Name:    htypes.Ptr("child"),
				}.Build(),
				Driver: htypes.Ptr("bash"),
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{`echo hello > $OUT`}),
					"out": hstructpb.NewStringsValue([]string{"out_child"}),
				},
			}.Build(),
		},
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(""),
					Name:    htypes.Ptr("parent"),
				}.Build(),
				Driver: htypes.Ptr("bash"),
				Config: map[string]*structpb.Value{
					"run":  hstructpb.NewStringsValue([]string{`echo $(cat $SRC) > $OUT`}),
					"deps": hstructpb.NewStringsValue([]string{"//:child"}),
					"out":  hstructpb.NewStringsValue([]string{"out_parent"}),
				},
			}.Build(),
		},
	})

	_, err = e.RegisterProvider(ctx, staticprovider, engine.RegisterProviderConfig{})
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, pluginexec.NewExec(), nil)
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
		rs, clean := e.NewRequestState()
		defer clean()

		res, err := e.Result(ctx, rs, "", "parent", []string{engine.AllOutputs})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		assertOut(res)
		res.Unlock(ctx)
	}

	{ // this should reuse cache from deps
		rs, clean := e.NewRequestState()
		defer clean()

		res, err := e.Result(ctx, rs, "", "parent", []string{engine.AllOutputs})
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
		Return(pluginv1.ProviderConfigResponse_builder{Name: htypes.Ptr("test_provider")}.Build(), nil).AnyTimes()

	provider.EXPECT().
		Get(gomock.Any(), gomock.Any()).
		DoAndReturn(func(ctx context.Context, request *pluginv1.GetRequest) (*pluginv1.GetResponse, error) {
			return pluginv1.GetResponse_builder{
				Spec: pluginv1.TargetSpec_builder{
					Ref:    pluginv1.TargetRef_builder{Package: htypes.Ptr(""), Name: htypes.Ptr("child")}.Build(),
					Driver: htypes.Ptr("test_driver"),
				}.Build(),
			}.Build(), nil
		}).Times(3)

	_, err = e.RegisterProvider(ctx, provider, engine.RegisterProviderConfig{})
	require.NoError(t, err)

	driver := pluginsdk.NewMockDriver(c)

	driver.EXPECT().
		Config(gomock.Any(), gomock.Any()).
		Return(pluginv1.ConfigResponse_builder{Name: htypes.Ptr("test_driver")}.Build(), nil).AnyTimes()

	driver.EXPECT().
		Parse(gomock.Any(), gomock.Any()).
		Return(pluginv1.ParseResponse_builder{
			Target: pluginv1.TargetDef_builder{
				Ref:    pluginv1.TargetRef_builder{Package: htypes.Ptr(""), Name: htypes.Ptr("child")}.Build(),
				Inputs: nil,
				Outputs: []*pluginv1.TargetDef_Output{pluginv1.TargetDef_Output_builder{
					Group: htypes.Ptr(""),
					Paths: []*pluginv1.TargetDef_Path{pluginv1.TargetDef_Path_builder{
						FilePath: htypes.Ptr("out.txt"),
					}.Build()},
				}.Build()},
				Cache: htypes.Ptr(true),
			}.Build(),
		}.Build(), nil).Times(3)

	driver.EXPECT().
		Run(gomock.Any(), gomock.Any()).
		DoAndReturn(func(ctx context.Context, request *pluginv1.RunRequest) (*pluginv1.RunResponse, error) {
			return pluginv1.RunResponse_builder{
				Artifacts: []*pluginv1.Artifact{
					pluginv1.Artifact_builder{
						Group: htypes.Ptr(""),
						Name:  htypes.Ptr("out"),
						Type:  htypes.Ptr(pluginv1.Artifact_TYPE_OUTPUT),
						Raw: pluginv1.Artifact_ContentRaw_builder{
							Data: []byte("hello"),
							Path: htypes.Ptr("out.txt"),
						}.Build(),
					}.Build(),
				},
			}.Build(), nil
		}).Times(1)

	_, err = e.RegisterDriver(ctx, driver, nil)
	require.NoError(t, err)

	cache := pluginsdk.NewMockCache(c)

	cache.EXPECT().
		Get(gomock.Any(), "__child/e5d50f4b478a3687/manifest.v1.json").
		Return(nil, pluginsdk.ErrNotFound).Times(1)

	for _, key := range []string{"__child/e5d50f4b478a3687/manifest.v1.json", "__child/e5d50f4b478a3687/out_out.tar"} {
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
		rs, clean := e.NewRequestState()
		defer clean()

		res, err := e.Result(ctx, rs, "", "child", []string{engine.AllOutputs})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		assertOut(res)
		res.Unlock(ctx)
	}

	{ // this should reuse cache from deps
		rs, clean := e.NewRequestState()
		defer clean()

		res, err := e.Result(ctx, rs, "", "child", []string{engine.AllOutputs})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		assertOut(res)
		res.Unlock(ctx)
	}

	err = os.RemoveAll(filepath.Join(dir, ".heph"))
	require.NoError(t, err)

	{ // this should reuse cache from deps
		rs, clean := e.NewRequestState()
		defer clean()

		res, err := e.Result(ctx, rs, "", "child", []string{engine.AllOutputs})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		assertOut(res)
		res.Unlock(ctx)
	}
}
