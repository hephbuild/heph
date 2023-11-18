package patternsm

import (
	"github.com/stretchr/testify/assert"
	"testing"
)

func TestIncludes(t *testing.T) {
	tests := []struct {
		p1         string
		p2         string
		includes   bool
		intersects bool
	}{
		{"", "", true, true},
		{"*", "*", true, true},
		{"**", "*", true, true},
		{"hey*", "hello*", false, false},
		{"some/path", "some/path", true, true},
		{"some/*", "some/path", true, true},
		{"some/**", "some/path", true, true},
		{"some/**/*", "some/path", true, true},
		{"some/**/path", "some/path", true, true},
		{"some/**/path", "some/cool/path", true, true},
		{"some/**/*", "some/path", true, true},
		{"some/**/*", "some/*", true, true},
		{"some/**/*", "some/other/*", true, true},
		{"some/**/*", "some/other/**/*", true, true},
		{"some/**/*", "some/other/**/som*", true, true},
		{"some/p*h", "some/path", true, true},
		{"some/path", "some/p*h", false, true},
		{"some/path", "some/**", false, true},
		{"some/ot*/cool/**/path", "some/other/cool/path", true, true},
		{"some/ot*/cool/**/path", "some/*her/cool/**/path", false, true},
		{"some/ot*/cool/**/path", "some/*her/cool/**/*", false, true},
		{"some/**/cool/**/*", "some/*her/cool/**/*", true, true},
		{"some/package", "some/package/*/some", false, false},
	}
	for _, test := range tests {
		t.Run(test.p1+" "+test.p2, func(t *testing.T) {
			r1 := Includes(test.p1, test.p2)
			assert.Equal(t, test.includes, r1)

			r2 := Intersects(test.p1, test.p2)
			assert.Equal(t, test.intersects, r2)

			r3 := Intersects(test.p2, test.p1)
			assert.Equalf(t, r2, r3, "inconsistent")
		})
	}
}
