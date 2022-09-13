package utils

import (
	"github.com/stretchr/testify/assert"
	"testing"
)

func TestTargetParse(t *testing.T) {
	t.Parallel()

	tests := []struct {
		fqn string
	}{
		{"//thirdparty/go/github.com/Azure/azure-sdk-for-go:_go_mod_download_v32.0.0_incompatible"},
	}
	for _, test := range tests {
		t.Run(test.fqn, func(t *testing.T) {
			_, err := TargetParse("", test.fqn)
			assert.NoError(t, err)
		})
	}
}
