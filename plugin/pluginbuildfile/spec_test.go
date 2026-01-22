package pluginbuildfile

import (
	"reflect"
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"go.starlark.net/starlark"
)

func TestTransitiveSpecDeps_Unpack(t *testing.T) {
	tests := []struct {
		name    string
		input   starlark.Value
		want    TransitiveSpecDeps
		wantErr bool
	}{
		{
			name:  "None value",
			input: starlark.None,
			want:  nil,
		},
		{
			name: "List of strings",
			input: starlark.NewList([]starlark.Value{
				starlark.String("dep1"),
				starlark.String("dep2"),
			}),
			want: TransitiveSpecDeps{
				"": {"dep1", "dep2"},
			},
		},
		{
			name:  "Single string",
			input: starlark.String("dep1"),
			want: TransitiveSpecDeps{
				"": {"dep1"},
			},
		},
		{
			name: "Dictionary of lists",
			input: func() starlark.Value {
				d := starlark.NewDict(2)
				_ = d.SetKey(starlark.String("pkg1"), starlark.NewList([]starlark.Value{starlark.String("a")}))
				_ = d.SetKey(starlark.String("pkg2"), starlark.NewList([]starlark.Value{starlark.String("b"), starlark.String("c")}))

				return d
			}(),
			want: TransitiveSpecDeps{
				"pkg1": {"a"},
				"pkg2": {"b", "c"},
			},
		},
		{
			name: "Dictionary with single string values",
			input: func() starlark.Value {
				d := starlark.NewDict(1)
				_ = d.SetKey(starlark.String("pkg1"), starlark.String("a"))

				return d
			}(),
			want: TransitiveSpecDeps{
				"pkg1": {"a"},
			},
		},
		{
			name:    "Invalid type (int)",
			input:   starlark.MakeInt(123),
			wantErr: true,
		},
		{
			name: "Invalid dictionary value type",
			input: func() starlark.Value {
				d := starlark.NewDict(1)
				_ = d.SetKey(starlark.String("pkg1"), starlark.MakeInt(123))

				return d
			}(),
			wantErr: true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			var got TransitiveSpecDeps
			err := got.Unpack(tt.input)

			if tt.wantErr {
				require.Error(t, err)
				return
			}

			require.NoError(t, err)
			assert.True(t, reflect.DeepEqual(tt.want, got), "expected %v, got %v", tt.want, got)
		})
	}
}
