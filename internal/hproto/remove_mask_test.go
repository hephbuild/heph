package hproto

import (
	"testing"

	"github.com/hephbuild/heph/internal/hmaps"

	"google.golang.org/protobuf/encoding/protojson"

	hprotov1 "github.com/hephbuild/heph/internal/hproto/internal/gen/heph/hproto/v1"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestRemoveMasked(t *testing.T) {
	m := &hprotov1.Sample{
		Included: []string{"hello"},
		Excluded: []string{"world"},
		Nested: &hprotov1.Sample_Nested{
			Included: []string{"hello"},
			Excluded: []string{"world"},
		},
		RepeatedNested: []*hprotov1.Sample_Nested{{
			Included: []string{"hello"},
			Excluded: []string{"world"},
		}},
	}

	m2, err := RemoveMasked(m, hmaps.Keyed([]string{"excluded", "nested.excluded", "repeated_nested.excluded"}))
	require.NoError(t, err)

	b, err := protojson.Marshal(m2)
	require.NoError(t, err)
	assert.JSONEq(t, `{"included":["hello"],"nested":{"included":["hello"]},"repeatedNested":[{"included":["hello"]}]}`, string(b))
}

func TestRemoveNil(t *testing.T) {
	m := &hprotov1.Sample{}

	_, err := RemoveMasked(m, hmaps.Keyed([]string{"excluded", "nested.excluded", "repeated_nested.excluded"}))
	require.NoError(t, err)
}
