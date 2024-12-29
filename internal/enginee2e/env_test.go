package enginee2e

import (
	"context"
	"os"
	"testing"

	"github.com/hephbuild/hephv2/internal/engine"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/hephv2/plugin/pluginexec"
	"github.com/hephbuild/hephv2/plugin/pluginstaticprovider"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"google.golang.org/protobuf/types/known/structpb"
)

func TestEnv(t *testing.T) {
	ctx := context.Background()

	dir, err := os.MkdirTemp("", "")
	require.NoError(t, err)
	defer os.RemoveAll(dir)

	e, err := engine.New(ctx, dir, engine.Config{})
	require.NoError(t, err)

	staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "no_out",
					Driver:  "sh",
				},
				Config: map[string]*structpb.Value{
					"run": newValueMust([]any{`echo hello`}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "unamed_out",
					Driver:  "sh",
				},
				Config: map[string]*structpb.Value{
					"run": newValueMust([]any{`echo hello > $OUT`}),
					"out": newValueMust([]any{"out"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_named_out",
					Driver:  "sh",
				},
				Config: map[string]*structpb.Value{
					"run": newValueMust([]any{`echo hello > $OUT_OUT1`}),
					"out": newValueMust(map[string]any{"out1": "out"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "two_named_out",
					Driver:  "sh",
				},
				Config: map[string]*structpb.Value{
					"run": newValueMust([]any{`echo hello > $OUT_OUT1`, `echo world > $OUT_OUT2`}),
					"out": newValueMust(map[string]any{"out1": "out1", "out2": "out1"}),
				},
			},
		},

		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_unamed_dep_no_out",
					Driver:  "sh",
				},
				Config: map[string]*structpb.Value{
					"run":  newValueMust([]any{`env | grep -v SRC`}),
					"deps": newValueMust([]any{"//:no_out"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_unamed_dep_unamed_out",
					Driver:  "sh",
				},
				Config: map[string]*structpb.Value{
					"run":  newValueMust([]any{`echo $SRC`}),
					"deps": newValueMust([]any{"//:unamed_out"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_unamed_dep_one_named_out_unspecified",
					Driver:  "sh",
				},
				Config: map[string]*structpb.Value{
					"run":  newValueMust([]any{`echo $SRC_OUT1`}),
					"deps": newValueMust([]any{"//:one_named_out"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_unamed_dep_one_named_out_specified",
					Driver:  "sh",
				},
				Config: map[string]*structpb.Value{
					"run":  newValueMust([]any{`echo $SRC`}),
					"deps": newValueMust([]any{"//:one_named_out|out1"}),
				},
			},
		},

		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_named_dep_no_out",
					Driver:  "sh",
				},
				Config: map[string]*structpb.Value{
					"run":  newValueMust([]any{`env | grep -v SRC`}),
					"deps": newValueMust(map[string]any{"in1": "//:no_out"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_named_dep_unamed_out",
					Driver:  "sh",
				},
				Config: map[string]*structpb.Value{
					"run":  newValueMust([]any{`echo $SRC_IN1`}),
					"deps": newValueMust(map[string]any{"in1": "//:unamed_out"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_named_dep_one_named_out_unspecified",
					Driver:  "sh",
				},
				Config: map[string]*structpb.Value{
					"run":  newValueMust([]any{`echo $SRC_IN1_OUT1`}),
					"deps": newValueMust(map[string]any{"in1": "//:one_named_out"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_named_dep_one_named_out_specified",
					Driver:  "sh",
				},
				Config: map[string]*structpb.Value{
					"run":  newValueMust([]any{`echo $SRC_IN1`}),
					"deps": newValueMust(map[string]any{"in1": "//:one_named_out|out1"}),
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

	{
		tests := []struct {
			target   string
			expected []string
		}{
			{"no_out", []string{}},
			{"unamed_out", []string{""}},
			{"one_named_out", []string{"out1"}},
			{"two_named_out", []string{"out1", "out2"}},
		}
		for _, test := range tests {
			t.Run(test.target, func(t *testing.T) {
				ch := e.Result(ctx, "", test.target, []string{engine.AllOutputs}, engine.ResultOptions{})

				res := <-ch
				require.NoError(t, res.Err)

				require.Len(t, res.Outputs, len(test.expected)+1)

				for _, name := range test.expected {
					found := false
					for _, output := range res.Outputs {
						if output.Group == name {
							found = true
							break
						}
					}

					if !found {
						assert.Failf(t, "output not found", "output %v not found", name)
					}
				}
			})
		}
	}

	{
		tests := []struct {
			target string
		}{
			{"one_unamed_dep_no_out"},
			{"one_unamed_dep_unamed_out"},
			{"one_unamed_dep_one_named_out_unspecified"},
			{"one_unamed_dep_one_named_out_specified"},

			{"one_named_dep_no_out"},
			{"one_named_dep_unamed_out"},
			{"one_named_dep_one_named_out_unspecified"},
			{"one_named_dep_one_named_out_specified"},
		}
		for _, test := range tests {
			t.Run(test.target, func(t *testing.T) {
				ch := e.Result(ctx, "", test.target, []string{}, engine.ResultOptions{})

				res := <-ch
				require.NoError(t, res.Err)
			})
		}
	}
}
