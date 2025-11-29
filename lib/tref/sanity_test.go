package tref

import (
	"testing"

	"github.com/hephbuild/heph/lib/tref/internal"

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
		{`//some/@foo:name@key=value`},
		{`//some/@foo:name@@key1=value,@key2=value`},
		{`//@heph/file/mgmt/go/lib/bskms:content@f="{gcp.go,interface.go}"`},
	}
	for _, test := range tests {
		t.Run(test.ref, func(t *testing.T) {
			internal.LexDebug(test.ref, false)

			actual, err := Parse(test.ref)
			require.NoError(t, err)
			_, err = ParseWithOut(test.ref)
			require.NoError(t, err)

			fmted := Format(actual)
			require.Equal(t, test.ref, fmted)
		})
	}
}

func TestSanityInPackage(t *testing.T) {
	tests := []struct {
		ref      string
		expected string
	}{
		{"//:name", "//:name"},
		{":name", "//some/pkg:name"},
	}
	for _, test := range tests {
		t.Run(test.ref, func(t *testing.T) {
			internal.LexDebug(test.ref, false)

			actual, err := ParseInPackage(test.ref, "some/pkg")
			require.NoError(t, err)

			fmted := Format(actual)
			require.Equal(t, test.expected, fmted)
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
		{"//some:name@key=value"},
		{"//some:name@key=value|out"},
		{"//some:name@key1=value1,key2=value2|out"},
		{"//some:name@key1=|out"},
		{"//some:name@@key1=value|out"},
		{"//some:name@@key1=value|out filters=foo"},
		{"//some:name@@key1=value|out filters=foo,bar"},
		{"//some:target filters=a,@b"},
	}
	for _, test := range tests {
		t.Run(test.ref, func(t *testing.T) {
			actual, err := ParseWithOut(test.ref)
			if err != nil {
				internal.LexDebug(test.ref, false)
			}
			require.NoError(t, err)

			t.Log(actual)

			fmted := FormatOut(actual)
			require.Equal(t, test.ref, fmted)
		})
	}
}
