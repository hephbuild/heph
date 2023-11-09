package hbuiltin

import (
	"encoding/json"
	"flag"
	"fmt"
	"github.com/hephbuild/heph/buildfiles"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
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

			_ = f.Close()

			parts := strings.SplitN(string(b), "===\n", 2)
			build := strings.TrimSpace(parts[0])
			expected := strings.TrimSpace(parts[1])
			for _, r := range replacements {
				expected = strings.ReplaceAll(expected, r.Spec, r.Actual)
			}

			f, err = os.CreateTemp("", "")
			require.NoError(t, err)
			defer os.Remove(f.Name())
			_, err = io.Copy(f, strings.NewReader(build))
			require.NoError(t, err)
			_ = f.Close()

			root, err := hroot.NewState("/tmp")
			require.NoError(t, err)

			pkgs := packages.NewRegistry(packages.Registry{
				Root: root,
			})
			bfs := buildfiles.NewState(buildfiles.State{
				Packages: pkgs,
			})

			pkg := &packages.Package{
				Path: "some/test",
				Root: xfs.NewPath(
					"/tmp/some/test",
					"some/test",
				),
			}

			var spec specs.Target

			opts := Bootstrap(Opts{
				Pkgs:   pkgs,
				Root:   root,
				Config: &config.Config{},
				RegisterTarget: func(rspec specs.Target) error {
					spec = rspec

					return nil
				},
			})

			err = bfs.RunBuildFile(pkg, f.Name(), opts)
			require.NoError(t, err)

			spec.Sources = nil

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

func TestDocFromArgs(t *testing.T) {
	tests := []struct {
		doc      string
		expected string
	}{
		{
			"\n \nsome title\n\nsome content:\n  - with\n  - bullets\n",
			"some title\n\nsome content:\n  - with\n  - bullets\n",
		},
		{
			"\n \n    some title\n\n    some content:\n      - with\n      - bullets\n",
			"some title\n\nsome content:\n  - with\n  - bullets\n",
		},
		{
			"\n \n\tsome title\n\n\tsome content:\n\t  - with\n\t  - bullets\n",
			"some title\n\nsome content:\n  - with\n  - bullets\n",
		},
	}
	for _, test := range tests {
		t.Run(strings.NewReplacer("\n", `\n`, " ", `\s`, "\t", `\t`).Replace(test.doc), func(t *testing.T) {
			actual := docFromArg(test.doc)

			assert.Equal(t, test.expected, actual)
		})
	}
}
