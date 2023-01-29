package targetspec

import (
	"fmt"
	"github.com/stretchr/testify/assert"
	"testing"
)

func TestTargetParse(t *testing.T) {
	t.Parallel()

	tests := []struct {
		fqn  string
		pkg  string
		name string
	}{
		{
			"//some/path:target",
			"some/path",
			"target",
		},
		{
			":target",
			"some/path",
			"target",
		},
		{
			"//thirdparty/go/github.com/Azure/azure-sdk-for-go:_go_mod_download_v32.0.0_incompatible",
			"thirdparty/go/github.com/Azure/azure-sdk-for-go",
			"_go_mod_download_v32.0.0_incompatible",
		},
	}
	for _, test := range tests {
		t.Run(test.fqn, func(t *testing.T) {
			tp, err := TargetParse(test.pkg, test.fqn)
			assert.NoError(t, err)
			assert.Equal(t, test.pkg, tp.Package)
			assert.Equal(t, test.name, tp.Name)
		})
	}
}

func TestTargetParseError(t *testing.T) {
	t.Parallel()

	tests := []struct {
		fqn   string
		error string
	}{
		{
			"//some/path::",
			"invalid target, got multiple `:`",
		},
		{
			"//some/path: test",
			"target name must match:",
		},
		{
			"//some/path:test:",
			"invalid target, got multiple `:`",
		},
	}
	for _, test := range tests {
		t.Run(test.fqn, func(t *testing.T) {
			_, err := TargetParse("", test.fqn)
			assert.ErrorContains(t, err, test.error)
		})
	}
}

func TestTargetOutputParse(t *testing.T) {
	t.Parallel()

	tests := []struct {
		fqn    string
		pkg    string
		name   string
		output string
	}{
		{
			"//some/path:target",
			"some/path",
			"target",
			"",
		},
		{
			":target",
			"some/path",
			"target",
			"",
		},
		{
			"//some/path:target|output",
			"some/path",
			"target",
			"output",
		},
	}
	for _, test := range tests {
		t.Run(test.fqn, func(t *testing.T) {
			tp, err := TargetOutputParse(test.pkg, test.fqn)
			assert.NoError(t, err)
			assert.Equal(t, test.pkg, tp.Package)
			assert.Equal(t, test.name, tp.Name)
			assert.Equal(t, test.output, tp.Output)
		})
	}
}

func TestTargetOutputParseError(t *testing.T) {
	t.Parallel()

	tests := []struct {
		fqn   string
		error string
	}{
		{
			"//some/path:test||",
			"package name must match",
		},
		{
			"//some/path:test|test|",
			"package name must match",
		},
	}
	for _, test := range tests {
		t.Run(test.fqn, func(t *testing.T) {
			_, _, err := TargetOutputOptionsParse("", test.fqn)
			assert.ErrorContains(t, err, test.error)
		})
	}
}

func TestTargetOutputOptionsParse(t *testing.T) {
	t.Parallel()

	tests := []struct {
		fqn     string
		pkg     string
		name    string
		output  string
		options map[string]string
	}{
		{
			"{mode=rw}//some/path:target",
			"some/path",
			"target",
			"",
			map[string]string{"mode": "rw"},
		},
		{
			"{}//some/path:target|output",
			"some/path",
			"target",
			"output",
			map[string]string{},
		},
		{
			"//some/path:target|output",
			"some/path",
			"target",
			"output",
			map[string]string{},
		},
	}
	for _, test := range tests {
		t.Run(test.fqn, func(t *testing.T) {
			tp, options, err := TargetOutputOptionsParse("", test.fqn)
			assert.NoError(t, err)
			assert.Equal(t, test.pkg, tp.Package)
			assert.Equal(t, test.name, tp.Name)
			assert.Equal(t, test.output, tp.Output)
			assert.Equal(t, fmt.Sprint(test.options), fmt.Sprint(options))
		})
	}
}
