//nolint:lll
package enginee2e

import (
	"context"
	"testing"

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
		{"result :a", func(t *testing.T, ctx context.Context, e *engine.Engine, pkg string, rs *engine.RequestState) {
			_, err := e.Result(ctx, rs, pkg, "c", []string{""})
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
							Name:    htypes.Ptr("c"),
						}.Build(),
						Driver: htypes.Ptr("sh"),
						Config: map[string]*structpb.Value{
							"deps": hstructpb.NewStringsValue([]string{"//@heph/query:query@label=gen"}),
						},
						Labels: []string{"gen"},
					}.Build(),
				},
			})

			_, err = e.RegisterProvider(ctx, staticprovider)
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
							// "deps": hstructpb.NewStringsValue([]string{"//@heph/query:query@label=gen"}),
							"deps": hstructpb.NewStringsValue([]string{"//" + pkg + ":a"}),
						},
					}.Build(),
				},
			})

			_, err = e.RegisterProvider(ctx, staticprovider)
			require.NoError(t, err)

			_, err = e.RegisterDriver(ctx, pluginexec.NewSh(), nil)
			require.NoError(t, err)

			_, err = e.RegisterDriver(ctx, plugingroup.New(), nil)
			require.NoError(t, err)

			rs, clean := e.NewRequestState()
			defer clean()

			test.do(t, ctx, e, pkg, rs)

			// _, err = e.ResultsFromMatcher(ctx, &pluginv1.TargetMatcher{Item: &pluginv1.TargetMatcher_Ref{Ref: &pluginv1.TargetRef{
			//	Package: pkg,
			//	Name:    "c",
			// }}}, rs)
			// require.ErrorContains(t, err, "stack recursion detected")

			// _, err = e.Result(ctx, pkg, "a", []string{""}, rs)
			// require.ErrorContains(t, err, "stack recursion detected")
			//
			// _, err = e.Result(ctx, pkg, "a", []string{""}, rs)
			// require.ErrorContains(t, err, "stack recursion detected")
			//
			// _, err = e.Result(ctx, pkg, "b", []string{""}, rs)
			// require.ErrorContains(t, err, "stack recursion detected")

			// _, err = e.Result(ctx, pkg, "c", []string{""}, rs)
			// require.ErrorContains(t, err, "stack recursion detected")

			// res := e.Query(ctx, &pluginv1.TargetMatcher{Item: &pluginv1.TargetMatcher_PackagePrefix{PackagePrefix: ""}}, rs)
			// var theErr error
			// var matches int
			// for _, err := range res {
			//	if err != nil {
			//		theErr = err
			//	} else {
			//		matches++
			//	}
			//}
			// require.Equal(t, 3, matches)
			// require.ErrorContains(t, theErr, "stack recursion detected")

			_, err = e.ResultsFromMatcher(ctx, rs, pluginv1.TargetMatcher_builder{PackagePrefix: proto.String("")}.Build())
			require.ErrorContains(t, err, "stack recursion detected")
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
							// "deps": hstructpb.NewStringsValue([]string{"//@heph/query:query@label=gen"}),
							"deps": hstructpb.NewStringsValue([]string{"//" + pkg + ":a"}),
						},
					}.Build(),
				},
			})

			_, err = e.RegisterProvider(ctx, staticprovider)
			require.NoError(t, err)

			_, err = e.RegisterDriver(ctx, pluginexec.NewSh(), nil)
			require.NoError(t, err)

			_, err = e.RegisterDriver(ctx, plugingroup.New(), nil)
			require.NoError(t, err)

			rs, clean := e.NewRequestState()
			defer clean()

			test.do(t, ctx, e, pkg, rs)

			_, err = e.ResultsFromMatcher(ctx, rs, pluginv1.TargetMatcher_builder{PackagePrefix: proto.String("")}.Build())
			require.ErrorContains(t, err, "stack recursion detected")
		})
	}
}
