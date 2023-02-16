package engine

import (
	"encoding/json"
	"flag"
	"fmt"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"go.starlark.net/starlark"
	"heph/packages"
	"heph/targetspec"
	fs2 "heph/utils/fs"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

var testWriteExpected = flag.Bool("write-expected", false, "Write expected output files")

type repl struct {
	Spec   string
	Actual string
}

var replacements = []repl{
	{Spec: "<OS>", Actual: runtime.GOOS},
	{Spec: "<ARCH>", Actual: runtime.GOARCH},
}

func TestTargetSpec(t *testing.T) {
	files := make([]string, 0)
	err := filepath.WalkDir("testdata", func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}

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

			f.Close()

			parts := strings.SplitN(string(b), "===\n", 2)
			build := strings.TrimSpace(parts[0])
			expected := strings.TrimSpace(parts[1])
			for _, r := range replacements {
				expected = strings.ReplaceAll(expected, r.Spec, r.Actual)
			}

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

			thread := newStarlarkThread()
			thread.SetLocal("engine", e)

			predeclaredGlobalsOnce(nil)

			_, err = starlark.ExecFile(thread, file, build, predeclared(predeclaredGlobals))
			require.NoError(t, err)

			spec.Source = nil

			actualb, err := json.MarshalIndent(spec, "", "    ")
			require.NoError(t, err)

			actual := string(actualb)

			if *testWriteExpected {
				f, err := os.Create(file)
				require.NoError(t, err)

				defer f.Close()

				actual := actual
				for _, r := range replacements {
					actual = strings.ReplaceAll(actual, r.Actual, r.Spec)
				}

				_, err = fmt.Fprintf(f, "%v\n===\n%v", build, actual)
				require.NoError(t, err)
			}

			t.Log(actual)

			assert.JSONEq(t, expected, actual)
		})
	}
}
