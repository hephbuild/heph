package cmd

import (
	"github.com/stretchr/testify/assert"
	"heph/targetspec"
	"heph/utils"
	"testing"
)

func TestAutocomplete(t *testing.T) {
	tests := []struct {
		name     string
		complete string
		targets  []string
		labels   []string
		expected []string
	}{
		{"sanity", "", []string{"//path/to:target"}, nil, []string{"//path/to:target"}},
		{"target name", "target", []string{"//path/to:target"}, nil, []string{"//path/to:target"}},
		{"fuzzy path target", "pathtarget", []string{"//path/to:target"}, nil, []string{"//path/to:target"}},
		{"fuzzy path target and separators", "//path:target", []string{"//path/to:target"}, nil, []string{"//path/to:target"}},

		{"fuzzy underscore", "buildamd", []string{"//path/to:build_amd64", "//path/to:build_arm64"}, nil, []string{"//path/to:build_amd64"}},
		{"fuzzy underscore", "buildamd", []string{"//:build_amd64", "//:build_arm64"}, nil, []string{"//:build_amd64"}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			t.Parallel()

			targets := utils.Map(test.targets, func(fqn string) targetspec.TargetSpec {
				return targetspec.TargetSpec{FQN: fqn}
			})

			_, suggestions := autocompleteLabelOrTarget(targets, test.labels, test.complete)

			assert.EqualValues(t, test.expected, suggestions)
		})
	}
}
