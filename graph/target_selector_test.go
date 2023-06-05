package graph

import (
	"fmt"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/tgt"
	"github.com/stretchr/testify/assert"
	"testing"
)

func targetFactory(pkg string, fqn string, labels []string) *Target {
	return &Target{
		Target: &tgt.Target{
			TargetSpec: targetspec.TargetSpec{
				FQN:    "//" + pkg + ":" + fqn,
				Labels: labels,
				Package: &packages.Package{
					Path: pkg,
				},
			},
		},
	}
}

func TestParseTargetSelector(t *testing.T) {
	t1 := targetFactory("some/pkg", "t1", []string{"label1", "label2"})
	t2 := targetFactory("some/pkg/deep", "t2", []string{"label1", "label2"})

	tests := []struct {
		selector string
		t        *Target
		expected bool
	}{
		{"//some/pkg:t1", t1, true},
		{"label1", t1, true},
		{"label2", t1, true},
		{"label3", t1, false},
		{"//some/pkg/.", t1, true},
		{"//some/pkg/.", t2, false},
		{"//some/pkg/...", t1, true},
		{"//some/pkg/...", t2, true},
	}
	for _, test := range tests {
		t.Run(fmt.Sprintf("%v %v", test.selector, test.t.FQN), func(t *testing.T) {
			m := ParseTargetSelector("", test.selector)

			assert.Equal(t, test.expected, m(test.t))
		})
	}
}