package tref

import (
	"testing"

	"github.com/stretchr/testify/require"
)

func TestSanity(t *testing.T) {
	tests := []struct {
		ref string
	}{
		{"//:name"},
		{"//some:name"},
		{"//some:name@key=value"},
		{"//some:name@key1=value1,key2=value2"},
		{`//some:name@key1="some \"cool\" | value, very 'complicated'"`},
	}
	for _, test := range tests {
		t.Run(test.ref, func(t *testing.T) {
			actual, err := Parse(test.ref)
			require.NoError(t, err)
			_, err = ParseWithOut(test.ref)
			require.NoError(t, err)

			fmted := Format(actual)
			require.Equal(t, test.ref, fmted)
		})
	}
}

func TestSanityOut(t *testing.T) {
	tests := []struct {
		ref string
	}{
		{"//:name"},
		{"//:name|out"},
		{"//some:name|out"},
		{"//some:name@key=value|out"},
		{"//some:name@key1=value1,key2=value2|out"},
	}
	for _, test := range tests {
		t.Run(test.ref, func(t *testing.T) {
			actual, err := ParseWithOut(test.ref)
			require.NoError(t, err)

			fmted := Format(actual)
			require.Equal(t, test.ref, fmted)
		})
	}
}
