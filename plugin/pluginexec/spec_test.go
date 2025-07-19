package pluginexec

import (
	"encoding/json"
	"testing"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"

	"github.com/stretchr/testify/require"
)

func decoder[T any]() func(any) (any, error) {
	return func(v any) (any, error) {
		v, err := hstructpb.Decode[T](v)
		return v, err
	}
}

func TestDecodeSpec(t *testing.T) {
	tests := []struct {
		value    any
		expected any
		decode   func(any) (any, error)
	}{
		{
			value:    nil,
			expected: nil,
			decode:   decoder[SpecStrings](),
		},
		{
			value:    "",
			expected: []string{""},
			decode:   decoder[SpecStrings](),
		},
		{
			value:    "hello",
			expected: []string{"hello"},
			decode:   decoder[SpecStrings](),
		},
		{
			value:    []string{"hello", "world"},
			expected: []string{"hello", "world"},
			decode:   decoder[SpecStrings](),
		},

		{
			value:    nil,
			expected: nil,
			decode:   decoder[SpecDeps](),
		},
		{
			value:    "foo",
			expected: SpecDeps{"": []string{"foo"}},
			decode:   decoder[SpecDeps](),
		},
		{
			value:    []string{"foo"},
			expected: SpecDeps{"": []string{"foo"}},
			decode:   decoder[SpecDeps](),
		},
		{
			value:    map[string]any{"hello": []string{"foo"}},
			expected: SpecDeps{"hello": []string{"foo"}},
			decode:   decoder[SpecDeps](),
		},
	}
	for _, test := range tests {
		t.Run("", func(t *testing.T) {
			actual, err := test.decode(test.value)
			require.NoError(t, err)

			expectedb, err := json.Marshal(test.expected)
			require.NoError(t, err)

			actualb, err := json.Marshal(actual)
			require.NoError(t, err)

			require.JSONEq(t, string(expectedb), string(actualb))
		})
	}
}
