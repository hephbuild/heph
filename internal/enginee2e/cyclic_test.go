//nolint:lll
package enginee2e

import (
	"context"
	"fmt"
	"runtime"
	"testing"
	"time"

	plugincyclicprovider "github.com/hephbuild/heph/internal/enginee2e/pluginscyclicprovider"
	"github.com/hephbuild/heph/internal/tmatch"
	"github.com/hephbuild/heph/lib/tref"
	"github.com/stretchr/testify/assert"

	"github.com/hephbuild/heph/internal/htypes"

	"github.com/hephbuild/heph/internal/hdebug"

	"github.com/go-faker/faker/v4"
	"github.com/hephbuild/heph/plugin/plugingroup"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"

	"github.com/hephbuild/heph/internal/engine"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/pluginexec"
	"github.com/hephbuild/heph/plugin/pluginstaticprovider"
	"github.com/stretchr/testify/require"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/known/structpb"
)

func TestCyclic1(t *testing.T) {
	tests := []struct {
		name string
		do   func(t *testing.T, ctx context.Context, e *engine.Engine, pkg string, rs *engine.RequestState)
	}{
		{"link all", func(t *testing.T, ctx context.Context, e *engine.Engine, pkg string, rs *engine.RequestState) {
			err := e.QueryLink(ctx, rs, tmatch.All(), tmatch.All())
			require.NoError(t, err)
		}},
		{"result :c", func(t *testing.T, ctx context.Context, e *engine.Engine, pkg string, rs *engine.RequestState) {
			res, err := e.Result(ctx, rs, pkg, "c", []string{""})
			defer res.Unlock(ctx)
			require.NoError(t, err)
		}},
		{"ResultsFromMatcher pkg prefix:", func(t *testing.T, ctx context.Context, e *engine.Engine, pkg string, rs *engine.RequestState) {
			res, err := e.ResultsFromMatcher(ctx, rs, pluginv1.TargetMatcher_builder{PackagePrefix: proto.String("")}.Build())
			defer res.Unlock(ctx)
			require.NoError(t, err)
		}},
		{"ResultsFromMatcher ref then ResultsFromMatcher pkg prefix:", func(t *testing.T, ctx context.Context, e *engine.Engine, pkg string, rs *engine.RequestState) {
			res, err := e.ResultsFromMatcher(ctx, rs, pluginv1.TargetMatcher_builder{Ref: pluginv1.TargetRef_builder{
				Package: htypes.Ptr(pkg),
				Name:    htypes.Ptr("c"),
			}.Build()}.Build())
			defer res.Unlock(ctx)
			require.NoError(t, err)

			res, err = e.ResultsFromMatcher(ctx, rs, pluginv1.TargetMatcher_builder{PackagePrefix: proto.String("")}.Build())
			defer res.Unlock(ctx)
			require.NoError(t, err)
		}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			ctx := t.Context()

			ctx, clean := hdebug.SetLabels(ctx, func() []string {
				return []string{"where", "test main"}
			})
			defer clean()

			dir := t.TempDir()

			e, err := engine.New(ctx, dir, engine.Config{})
			require.NoError(t, err)

			pkg := faker.UUIDDigit()

			staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
				{
					Spec: pluginv1.TargetSpec_builder{
						Ref: pluginv1.TargetRef_builder{
							Package: htypes.Ptr(pkg),
							Name:    htypes.Ptr("c"),
						}.Build(),
						Driver: htypes.Ptr("sh"),
						Config: map[string]*structpb.Value{
							"deps": structpb.NewStringValue(tref.FormatQuery(tref.QueryOptions{
								Label:        "gen",
								SkipProvider: pluginstaticprovider.Name,
							})),
						},
						Labels: []string{"gen"},
					}.Build(),
				},
			})

			_, err = e.RegisterProvider(ctx, staticprovider, engine.RegisterProviderConfig{})
			require.NoError(t, err)

			_, err = e.RegisterDriver(ctx, pluginexec.NewSh(), nil)
			require.NoError(t, err)

			_, err = e.RegisterDriver(ctx, plugingroup.New(), nil)
			require.NoError(t, err)

			rs, clean := e.NewRequestState()
			defer clean()

			test.do(t, ctx, e, pkg, rs)
		})
	}
}

func TestCyclic2(t *testing.T) {
	tests := []struct {
		name string
		do   func(t *testing.T, ctx context.Context, e *engine.Engine, pkg string, rs *engine.RequestState)
	}{
		{"result :a", func(t *testing.T, ctx context.Context, e *engine.Engine, pkg string, rs *engine.RequestState) {
			_, err := e.Result(ctx, rs, pkg, "a", []string{""})
			require.ErrorContains(t, err, "stack recursion detected")
		}},
		{"ResultsFromMatcher pkg prefix: <root>", func(t *testing.T, ctx context.Context, e *engine.Engine, pkg string, rs *engine.RequestState) {
			_, err := e.ResultsFromMatcher(ctx, rs, pluginv1.TargetMatcher_builder{PackagePrefix: proto.String("")}.Build())
			require.ErrorContains(t, err, "stack recursion detected")
		}},
		{"ResultsFromMatcher ref leaf then ResultsFromMatcher pkg prefix: <root>", func(t *testing.T, ctx context.Context, e *engine.Engine, pkg string, rs *engine.RequestState) {
			_, err := e.ResultsFromMatcher(ctx, rs, pluginv1.TargetMatcher_builder{Ref: pluginv1.TargetRef_builder{
				Package: htypes.Ptr(pkg),
				Name:    htypes.Ptr("c"),
			}.Build()}.Build())
			require.ErrorContains(t, err, "stack recursion detected")

			_, err = e.ResultsFromMatcher(ctx, rs, pluginv1.TargetMatcher_builder{PackagePrefix: proto.String("")}.Build())
			require.ErrorContains(t, err, "stack recursion detected")
		}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			ctx := t.Context()

			ctx, clean := hdebug.SetLabels(ctx, func() []string {
				return []string{"where", "test main"}
			})
			defer clean()

			dir := t.TempDir()

			e, err := engine.New(ctx, dir, engine.Config{})
			require.NoError(t, err)

			pkg := faker.UUIDDigit()

			staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
				{
					Spec: pluginv1.TargetSpec_builder{
						Ref: pluginv1.TargetRef_builder{
							Package: htypes.Ptr(pkg),
							Name:    htypes.Ptr("a"),
						}.Build(),
						Driver: htypes.Ptr("sh"),
						Config: map[string]*structpb.Value{
							"deps": hstructpb.NewStringsValue([]string{"//" + pkg + ":c"}),
						},
						Labels: []string{"gen"},
					}.Build(),
				},
				{
					Spec: pluginv1.TargetSpec_builder{
						Ref: pluginv1.TargetRef_builder{
							Package: htypes.Ptr(pkg),
							Name:    htypes.Ptr("c"),
						}.Build(),
						Driver: htypes.Ptr("sh"),
						Config: map[string]*structpb.Value{
							"deps": hstructpb.NewStringsValue([]string{"//" + pkg + ":a"}),
						},
					}.Build(),
				},
			})

			_, err = e.RegisterProvider(ctx, staticprovider, engine.RegisterProviderConfig{})
			require.NoError(t, err)

			_, err = e.RegisterDriver(ctx, pluginexec.NewSh(), nil)
			require.NoError(t, err)

			_, err = e.RegisterDriver(ctx, plugingroup.New(), nil)
			require.NoError(t, err)

			rs, clean := e.NewRequestState()
			defer clean()

			test.do(t, ctx, e, pkg, rs)
		})
	}
}

func TestCyclic3(t *testing.T) {
	tests := []struct {
		name string
		do   func(t *testing.T, ctx context.Context, e *engine.Engine, pkg string, rs *engine.RequestState)
	}{
		{"result :a", func(t *testing.T, ctx context.Context, e *engine.Engine, pkg string, rs *engine.RequestState) {
			_, err := e.Result(ctx, rs, pkg, "a", []string{""})
			require.ErrorContains(t, err, "stack recursion detected")
		}},
		{"ResultsFromMatcher pkg prefix:", func(t *testing.T, ctx context.Context, e *engine.Engine, pkg string, rs *engine.RequestState) {
			_, err := e.ResultsFromMatcher(ctx, rs, pluginv1.TargetMatcher_builder{PackagePrefix: proto.String("")}.Build())
			require.ErrorContains(t, err, "stack recursion detected")
		}},
		{"ResultsFromMatcher ref then ResultsFromMatcher pkg prefix:", func(t *testing.T, ctx context.Context, e *engine.Engine, pkg string, rs *engine.RequestState) {
			_, err := e.ResultsFromMatcher(ctx, rs, pluginv1.TargetMatcher_builder{Ref: pluginv1.TargetRef_builder{
				Package: htypes.Ptr(pkg),
				Name:    htypes.Ptr("c"),
			}.Build()}.Build())
			require.ErrorContains(t, err, "stack recursion detected")

			_, err = e.ResultsFromMatcher(ctx, rs, pluginv1.TargetMatcher_builder{PackagePrefix: proto.String("")}.Build())
			require.ErrorContains(t, err, "stack recursion detected")
		}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			ctx := t.Context()

			ctx, clean := hdebug.SetLabels(ctx, func() []string {
				return []string{"where", "test main"}
			})
			defer clean()

			dir := t.TempDir()

			e, err := engine.New(ctx, dir, engine.Config{})
			require.NoError(t, err)

			pkg := faker.UUIDDigit()

			staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
				{
					Spec: pluginv1.TargetSpec_builder{
						Ref: pluginv1.TargetRef_builder{
							Package: htypes.Ptr(pkg),
							Name:    htypes.Ptr("a"),
						}.Build(),
						Driver: htypes.Ptr("sh"),
						Config: map[string]*structpb.Value{
							"deps": hstructpb.NewStringsValue([]string{"//" + pkg + ":b"}),
						},
						Labels: []string{"gen"},
					}.Build(),
				},
				{
					Spec: pluginv1.TargetSpec_builder{
						Ref: pluginv1.TargetRef_builder{
							Package: htypes.Ptr(pkg),
							Name:    htypes.Ptr("b"),
						}.Build(),
						Driver: htypes.Ptr("sh"),
						Config: map[string]*structpb.Value{
							"deps": hstructpb.NewStringsValue([]string{"//" + pkg + ":a"}),
						},
						Labels: []string{"gen"},
					}.Build(),
				},
				{
					Spec: pluginv1.TargetSpec_builder{
						Ref: pluginv1.TargetRef_builder{
							Package: htypes.Ptr(pkg),
							Name:    htypes.Ptr("c"),
						}.Build(),
						Driver: htypes.Ptr("sh"),
						Config: map[string]*structpb.Value{
							"deps": hstructpb.NewStringsValue([]string{"//" + pkg + ":a"}),
						},
					}.Build(),
				},
			})

			_, err = e.RegisterProvider(ctx, staticprovider, engine.RegisterProviderConfig{})
			require.NoError(t, err)

			_, err = e.RegisterDriver(ctx, pluginexec.NewSh(), nil)
			require.NoError(t, err)

			_, err = e.RegisterDriver(ctx, plugingroup.New(), nil)
			require.NoError(t, err)

			rs, clean := e.NewRequestState()
			defer clean()

			test.do(t, ctx, e, pkg, rs)
		})
	}
}

func TestCyclic4(t *testing.T) {
	for _, test := range [][2]bool{{false, false}, {true, false}, {false, true}, {true, true}} {
		t.Run(fmt.Sprintf("get: %v list: %v", test[0], test[1]), func(t *testing.T) {
			ctx := t.Context()
			go func() {
				for {
					select {
					case <-ctx.Done():
						return
					case <-time.After(time.Second):
						if runtime.NumGoroutine() > 1000 {
							panic("too many goroutines")
						}
					}
				}
			}()

			ctx, clean := hdebug.SetLabels(ctx, func() []string {
				return []string{"where", "test main"}
			})
			defer clean()

			dir := t.TempDir()

			e, err := engine.New(ctx, dir, engine.Config{})
			require.NoError(t, err)

			e.WellKnownPackages = []string{"", "some", "some/package"}

			_, err = e.RegisterProvider(ctx, plugincyclicprovider.New(test[0], test[1]), engine.RegisterProviderConfig{})
			require.NoError(t, err)

			_, err = e.RegisterDriver(ctx, pluginexec.NewSh(), nil)
			require.NoError(t, err)

			_, err = e.RegisterDriver(ctx, plugingroup.New(), nil)
			require.NoError(t, err)

			rs, clean := e.NewRequestState()
			defer clean()

			res, err := e.ResultsFromMatcher(ctx, rs, pluginv1.TargetMatcher_builder{PackagePrefix: proto.String("")}.Build())
			require.NoError(t, err)
			defer res.Unlock(ctx)

			assert.Len(t, res, 2)
		})
	}
}
