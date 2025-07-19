package tmatch

import (
	"testing"

	"github.com/stretchr/testify/require"
)

func TestParsePackageMatcher(t *testing.T) {
	tests := []struct {
		s        string
		root     string
		cwd      string
		expected string
	}{
		{"./...", "/root", "/root", `package_prefix:""`},
		{"./...", "/root", "/root/foo", `package_prefix:"foo"`},
		{"...", "/root", "/root", `package_prefix:""`},
		{"...", "/root", "/root/foo", `package_prefix:""`},
		{"//...", "/root", "/root/foo", `package_prefix:""`},
		{".", "/root", "/root/foo", `package:"foo"`},
		{"./foo", "/root", "/root", `package:"foo"`},
		{"foo", "/root", "/root", `package:"foo"`},
		{"foo", "/root", "/root/foo", `package:"foo"`},
	}
	for _, test := range tests {
		t.Run(test.s+" "+test.root+" "+test.cwd+" "+test.expected, func(t *testing.T) {
			m, err := ParsePackageMatcher(test.s, test.cwd, test.root)
			require.NoError(t, err)

			require.Equal(t, test.expected, m.String())
		})
	}
}
