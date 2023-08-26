package main

import (
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/stretchr/testify/assert"
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
		{"path target prefix", "//path:target", []string{"//path/to:target"}, nil, nil},
		{"fuzzy path target and separators", "path/to:target", []string{"//path/to/some:target"}, nil, []string{"//path/to/some:target"}},

		{"fuzzy underscore", "buildamd", []string{"//path/to:build_amd64", "//path/to:build_arm64"}, nil, []string{"//path/to:build_amd64"}},
		{"fuzzy underscore", "buildamd", []string{"//:build_amd64", "//:build_arm64"}, nil, []string{"//:build_amd64"}},
	}
	for _, test := range tests {
		test := test

		t.Run(test.name, func(t *testing.T) {
			t.Parallel()

			targets := ads.Map(test.targets, func(addr string) specs.Target {
				return specs.Target{Addr: addr}
			})

			_, suggestions := autocompleteLabelOrTarget(targets, test.labels, test.complete)

			assert.EqualValues(t, test.expected, suggestions)
		})
	}
}
