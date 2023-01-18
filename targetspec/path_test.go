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
			"//thirdparty/go/github.com/Azure/azure-sdk-for-go:_go_mod_download_v32.0.0_incompatible",
			"thirdparty/go/github.com/Azure/azure-sdk-for-go",
			"_go_mod_download_v32.0.0_incompatible",
		},
	}
	for _, test := range tests {
		t.Run(test.fqn, func(t *testing.T) {
			tp, err := TargetParse("", test.fqn)
			assert.NoError(t, err)
			assert.Equal(t, test.pkg, tp.Package)
			assert.Equal(t, test.name, tp.Name)
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
			"//some/path:target|output",
			"some/path",
			"target",
			"output",
		},
	}
	for _, test := range tests {
		t.Run(test.fqn, func(t *testing.T) {
			tp, err := TargetOutputParse("", test.fqn)
			assert.NoError(t, err)
			assert.Equal(t, test.pkg, tp.Package)
			assert.Equal(t, test.name, tp.Name)
			assert.Equal(t, test.output, tp.Output)
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
