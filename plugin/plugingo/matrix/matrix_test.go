package matrix

import (
	"github.com/stretchr/testify/assert"
	"maps"
	"slices"
	"strings"
	"testing"
)

func sortresult(res []map[string]string) {
	slices.SortFunc(res, func(a, b map[string]string) int {
		keysi := slices.Sorted(maps.Keys(a))
		keysj := slices.Sorted(maps.Keys(b))

		if ci := len(keysi) - len(keysj); ci != 0 {
			return ci
		}

		for ki := range keysi {
			if ci := strings.Compare(keysi[ki], keysj[ki]); ci != 0 {
				return ci
			}

			if ci := strings.Compare(a[keysi[ki]], b[keysj[ki]]); ci != 0 {
				return ci
			}
		}

		return 0
	})
}

func TestGenerate(t *testing.T) {
	res := Generate([]Variable{
		{Key: "os", Values: []string{"linux", "darwin", "windows"}},
		{Key: "arch", Values: []string{"amd64", "arm64"}},
		{Key: "debug", Values: []string{"", "true"}},
	}, []map[string]string{
		{"os": "windows", "arch": "arm64"},
	})

	expected := []map[string]string{
		{
			"os":   "linux",
			"arch": "amd64",
		},
		{
			"os":   "darwin",
			"arch": "amd64",
		},
		{
			"os":   "windows",
			"arch": "amd64",
		},
		{
			"os":   "linux",
			"arch": "arm64",
		},
		{
			"os":   "darwin",
			"arch": "arm64",
		},
		{
			"os":    "linux",
			"arch":  "amd64",
			"debug": "true",
		},
		{
			"os":    "darwin",
			"arch":  "amd64",
			"debug": "true",
		},
		{
			"os":    "windows",
			"arch":  "amd64",
			"debug": "true",
		},
		{
			"os":    "linux",
			"arch":  "arm64",
			"debug": "true",
		},
		{
			"os":    "darwin",
			"arch":  "arm64",
			"debug": "true",
		},
	}

	sortresult(expected)
	sortresult(res)

	assert.EqualValues(t, expected, res)
}
