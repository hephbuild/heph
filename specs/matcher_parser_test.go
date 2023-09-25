package specs

import (
	"fmt"
	"github.com/hephbuild/heph/packages"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"strings"
	"testing"
)

func TestParseMatcher(t *testing.T) {
	tests := []struct {
		expr     string
		expected string
	}{
		{"test", "test"},
		{":test", "//**:test"},
		{":t*st", "//**:t*st"},
		{":", "//**:*"},
		{"test1 && test2", "(test1 && test2)"},
		{"test1 && !test2", "(test1 && !test2)"},
		{"!(test1 && test2)", "!(test1 && test2)"},
		{"test1 && (test2)", "(test1 && test2)"},
		{"test1 && (test2 && test3)", "(test1 && (test2 && test3))"},
		{"test1 || (test2)", "(test1 || test2)"},
		{"//path/to:target", "//path/to:target"},
		{"//path/to:target || label", "(//path/to:target || label)"},
		{"//path/to/...", "//path/to/**:*"},
		{"//path/to/.", "//path/to:*"},
		{"//path/to/**:*", "//path/to/**:*"},
		{"//path/to/**", "//path/to/**:*"},
		{"//path/to:*", "//path/to:*"},
		{"//path/to", "//path/to:*"},
		{`has_annotation("some")`, `has_annotation("some")`},
		{`label*`, `label*`},
	}
	for _, test := range tests {
		t.Run(test.expr, func(t *testing.T) {
			ast, err := ParseMatcher(test.expr)
			require.NoError(t, err)

			assert.Equal(t, test.expected, ast.String())
		})

		if strings.Contains(test.expr, " ") {
			t.Run(test.expr+" ~nospace", func(t *testing.T) {
				ast, err := ParseMatcher(strings.ReplaceAll(test.expr, " ", ""))
				require.NoError(t, err)

				assert.Equal(t, test.expected, ast.String())
			})
		}
	}
}

func TestParseMatcherErr(t *testing.T) {
	tests := []struct {
		expr     string
		expected string
	}{
		{"~", "Unexpected character ~"},
		{"//path/to:target label", "Unexpected token label: label"},
		{"//path/to/***:label", "unexpected *** in target path"},
		{"//path/to:**", "unexpected ** in target name"},
		{`has_annotation("some)`, "Expected comma, got EOF"},
		{`has_annotation("some'`, "Expected comma, got EOF"},
		{`label**`, "unexpected ** in label"},
	}
	for _, test := range tests {
		t.Run(test.expr, func(t *testing.T) {
			_, err := ParseMatcher(test.expr)
			assert.ErrorContains(t, err, test.expected)
		})
	}
}

func targetFactory(pkg string, name string, labels []string) Target {
	return Target{
		Name:   name,
		Addr:   "//" + pkg + ":" + name,
		Labels: labels,
		Package: &packages.Package{
			Path: pkg,
		},
	}
}

func TestMatch(t *testing.T) {
	t1 := targetFactory("some/pkg", "t1", []string{"label1", "label2"})
	t2 := targetFactory("some/pkg/deep", "t2", []string{"label1", "label2"})
	t3 := targetFactory("some/pkg/deep", "t3", []string{"label1", "label2"})

	tests := []struct {
		selector string
		t        Target
		expected bool
	}{
		{"//some/pkg:t1", t1, true},
		{":t1", t1, true},
		{"label1", t1, true},
		{"label1 && label2", t1, true},
		{"label1 && label2 && label3", t1, false},
		{"(label1 && label2) || label3", t1, true},
		{"label2", t1, true},
		{"label3", t1, false},
		{"//some/pkg:*", t1, true},
		{"//some/pkg:*", t2, false},
		{"//some/pkg/**", t1, true},
		{"//some/pkg/**", t2, true},
		{"//some/pkg/** && label1", t2, true},
		{"//some/pkg/deep:*2", t2, true},
		{"//some/pkg/deep:*2", t3, false},
		{"//some/pkg/d*p:*2", t2, true},
		{"//some/pkg/d*p:*2", t3, false},
		{"//some/**/d*p:*2", t2, true},
		{"//some/**/d*p:*2", t3, false},
		{"label*", t1, true},
		{"lab*2", t2, true},
		{"lab*4", t2, false},
		{"*2", t2, true},
	}
	for _, test := range tests {
		t.Run(fmt.Sprintf("%v %v", test.selector, test.t.Addr), func(t *testing.T) {
			t.Log("selector", test.selector)
			t.Log("target  ", test.t.Addr)

			m, _ := ParseMatcher(test.selector)

			assert.Equal(t, test.expected, m.Match(test.t))
		})
	}
}
