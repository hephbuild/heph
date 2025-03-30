package simpleregex

import (
	"testing"

	"github.com/stretchr/testify/require"
)

type M = map[string]string

func Test(t *testing.T) {
	tests := []struct {
		pattern  string
		str      string
		expected M
	}{
		{
			pattern:  "@go/thirdparty/{repo}@{version}",
			str:      "@go/thirdparty/github.com/repo/package@1.2.3",
			expected: M{"repo": "github.com/repo/package", "version": "1.2.3"},
		},
		{
			pattern:  "@go/thirdparty/{repo}@{version}/{package}",
			str:      "@go/thirdparty/github.com/repo/package@1.2.3/lib",
			expected: M{"repo": "github.com/repo/package", "version": "1.2.3", "package": "lib"},
		},
		{
			pattern:  "@go/thirdparty/{repo}@{version!/}/{package}",
			str:      "@go/thirdparty/github.com/repo/package@1.2.3/deep/lib",
			expected: M{"repo": "github.com/repo/package", "version": "1.2.3", "package": "deep/lib"},
		},
	}
	for _, test := range tests {
		t.Run(test.pattern+"_"+test.str, func(t *testing.T) {
			m, err := New(test.pattern)
			require.NoError(t, err)

			t.Log(m.String())

			res, ok := m.Match(test.str)
			require.True(t, ok)
			require.EqualValues(t, test.expected, res)
		})
	}
}
