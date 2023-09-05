package fmt

import (
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestFmt(t *testing.T) {
	files := make([]string, 0)
	err := filepath.Walk("testdata", func(path string, info fs.FileInfo, err error) error {
		if err != nil {
			return err
		}

		if info.IsDir() {
			return nil
		}

		files = append(files, path)

		return nil
	})
	require.NoError(t, err)

	cfg := Config{IndentSize: 2}

	for _, file := range files {
		t.Run(file, func(t *testing.T) {
			b, err := os.ReadFile(file)
			require.NoError(t, err)

			code, expected, _ := strings.Cut(string(b), "----\n")

			if strings.Contains(code, "# skip") {
				t.Skip("skip comment detected")
				return
			}

			formatted, err := fmtSource("BUILD", strings.NewReader(code), cfg)
			require.NoError(t, err)

			assert.Equal(t, expected, formatted)

			formatted2, err := fmtSource("BUILD", strings.NewReader(formatted), cfg)
			require.NoError(t, err)

			assert.Equal(t, formatted, formatted2, "formatting is not consistent")
		})
	}
}

func TestFmtSkipFile(t *testing.T) {
	tests := []struct {
		name     string
		code     string
		expected bool
	}{
		{
			"should skip",
			"# heph:fmt skip-file\na=b",
			true,
		},
		{
			"should not skip",
			"a=b\n# heph:fmt skip-file\nb=c",
			false,
		},
	}

	cfg := Config{IndentSize: 2}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			_, err := fmtSource("BUILD", strings.NewReader(test.code), cfg)
			if test.expected {
				require.ErrorContains(t, err, ErrSkip.Error())
			} else {
				require.NoError(t, err)
			}
		})
	}
}

func TestFormatComment(t *testing.T) {
	tests := []struct {
		comment, expected string
	}{
		{"# hello", "# hello"},
		{"#hello", "# hello"},
		{"#  hello", "#  hello"},
		{"#  ", "#"},
		{"#", "#"},
	}
	for _, test := range tests {
		t.Run(test.comment, func(t *testing.T) {
			actual := formatComment(test.comment)
			assert.Equal(t, test.expected, actual)
		})
	}
}
