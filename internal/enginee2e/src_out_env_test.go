package enginee2e

import (
	"testing"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"

	"github.com/hephbuild/heph/internal/engine"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/pluginexec"
	"github.com/hephbuild/heph/plugin/pluginstaticprovider"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"google.golang.org/protobuf/types/known/structpb"
)

func TestSrcOutEnv(t *testing.T) {
	ctx := t.Context()

	dir := t.TempDir()

	e, err := engine.New(ctx, dir, engine.Config{})
	require.NoError(t, err)

	staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "no_out",
				},
				Driver: "sh",
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{`echo hello`}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "unamed_out",
				},
				Driver: "sh",
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{`echo hello > $OUT`}),
					"out": hstructpb.NewStringsValue([]string{"out"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_named_out",
				},
				Driver: "sh",
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{`echo hello > $OUT_OUT1`}),
					"out": newValueMust(map[string]any{"out1": "out"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "two_named_out",
				},
				Driver: "sh",
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{`echo hello > $OUT_OUT1`, `echo world > $OUT_OUT2`}),
					"out": newValueMust(map[string]any{"out1": "out1", "out2": "out1"}),
				},
			},
		},

		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_unamed_dep_no_out",
				},
				Driver: "sh",
				Config: map[string]*structpb.Value{
					"run":  hstructpb.NewStringsValue([]string{`env | grep -v SRC`}),
					"deps": hstructpb.NewStringsValue([]string{"//:no_out"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_unamed_dep_unamed_out",
				},
				Driver: "sh",
				Config: map[string]*structpb.Value{
					"run":  hstructpb.NewStringsValue([]string{`echo $SRC`}),
					"deps": hstructpb.NewStringsValue([]string{"//:unamed_out"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_unamed_dep_one_named_out_unspecified",
				},
				Driver: "sh",
				Config: map[string]*structpb.Value{
					"run":  hstructpb.NewStringsValue([]string{`echo $SRC_OUT1`}),
					"deps": hstructpb.NewStringsValue([]string{"//:one_named_out"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_unamed_dep_one_named_out_specified",
				},
				Driver: "sh",
				Config: map[string]*structpb.Value{
					"run":  hstructpb.NewStringsValue([]string{`echo $SRC`}),
					"deps": hstructpb.NewStringsValue([]string{"//:one_named_out|out1"}),
				},
			},
		},

		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_named_dep_no_out",
				},
				Driver: "sh",
				Config: map[string]*structpb.Value{
					"run":  hstructpb.NewStringsValue([]string{`env | grep -v SRC`}),
					"deps": newValueMust(map[string]any{"in1": "//:no_out"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_named_dep_unamed_out",
				},
				Driver: "sh",
				Config: map[string]*structpb.Value{
					"run":  hstructpb.NewStringsValue([]string{`echo $SRC_IN1`}),
					"deps": newValueMust(map[string]any{"in1": "//:unamed_out"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_named_dep_one_named_out_unspecified",
				},
				Driver: "sh",
				Config: map[string]*structpb.Value{
					"run":  hstructpb.NewStringsValue([]string{`echo $SRC_IN1_OUT1`}),
					"deps": newValueMust(map[string]any{"in1": "//:one_named_out"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "one_named_dep_one_named_out_specified",
				},
				Driver: "sh",
				Config: map[string]*structpb.Value{
					"run":  hstructpb.NewStringsValue([]string{`echo $SRC_IN1`}),
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
				rs, clean := e.NewRequestState()
				defer clean()

				res, err := e.Result(ctx, rs, "", test.target, []string{engine.AllOutputs})
				require.NoError(t, err)
				defer res.Unlock(ctx)

				require.Len(t, res.Artifacts, len(test.expected))

				for _, name := range test.expected {
					outputArtifacts := res.FindOutputs(name)

					if len(outputArtifacts) != 1 {
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
				rs, clean := e.NewRequestState()
				defer clean()

				res, err := e.Result(ctx, rs, "", test.target, []string{})
				require.NoError(t, err)
				defer res.Unlock(ctx)
			})
		}
	}
}
