package engine

import (
	"encoding/json"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"go.starlark.net/starlark"
	"heph/packages"
	"heph/targetspec"
	fs2 "heph/utils/fs"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

func TestTargetSpec(t *testing.T) {
	files := make([]string, 0)
	err := filepath.WalkDir("testdata", func(path string, d fs.DirEntry, err error) error {
		if d.IsDir() {
			return nil
		}

		files = append(files, path)
		return nil
	})
	require.NoError(t, err)

	// Just sanity check
	assert.Equal(t, 7, len(files))

	for _, file := range files {
		t.Log(file)

		t.Run(file, func(t *testing.T) {
			f, err := os.Open(file)
			require.NoError(t, err)
			defer f.Close()

			b, err := io.ReadAll(f)
			require.NoError(t, err)

			parts := strings.SplitN(string(b), "===\n", 2)
			build := parts[0]
			expected := strings.TrimSpace(parts[1])

			lspath, err := exec.LookPath("ls")
			require.NoError(t, err)

			expected = strings.ReplaceAll(expected, "REPLACE_LS_BIN", lspath)

			var spec targetspec.TargetSpec

			e := &runBuildEngine{
				pkg: &packages.Package{
					Name:     "test",
					FullName: "some/test",
					Root: fs2.NewPath(
						"/tmp/some/test",
						"some/test",
					),
				},
				registerTarget: func(rspec targetspec.TargetSpec) error {
					spec = rspec

					return nil
				},
			}

			thread := &starlark.Thread{}
			thread.SetLocal("engine", e)

			predeclaredGlobalsOnce(nil)

			_, err = starlark.ExecFile(thread, file, build, predeclared(predeclaredGlobals))
			require.NoError(t, err)

			spec.Source = nil

			actual, err := json.MarshalIndent(spec, "", "    ")
			require.NoError(t, err)

			t.Log(string(actual))

			assert.JSONEq(t, expected, string(actual))
		})
	}
}
