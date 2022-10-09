package engine

import (
	"encoding/json"
	"fmt"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"go.starlark.net/starlark"
	"heph/exprs"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func genTargetSpec(name string, factor int) TargetSpec {
	pkg := &Package{
		Name:     "aaa",
		FullName: "aaa",
	}

	var deps TargetSpecDeps
	for i := 0; i < factor; i++ {
		deps.Targets = append(deps.Targets, TargetSpecDepTarget{
			Name:   "aaa",
			Output: "aaa",
			Target: "//aaa",
		})
		deps.Files = append(deps.Files, TargetSpecDepFile{
			Name: "aaa",
			Path: "aaa",
		})
		deps.Exprs = append(deps.Exprs, TargetSpecDepExpr{
			Name:    "aaa",
			Package: pkg,
			Expr: exprs.Expr{
				String:    "$(aaa)",
				Function:  "aaa",
				PosArgs:   nil,
				NamedArgs: nil,
			},
		})
	}

	var exprTools []TargetSpecExprTool
	for i := 0; i < factor; i++ {
		exprTools = append(exprTools, TargetSpecExprTool{
			Expr: exprs.Expr{
				String:    "$(aaa)",
				Function:  "aaa",
				PosArgs:   nil,
				NamedArgs: nil,
			},
			Output: "",
		})
	}

	var targetTools []TargetSpecTargetTool
	for i := 0; i < factor; i++ {
		targetTools = append(targetTools, TargetSpecTargetTool{
			Target: "//:aaa",
			Output: "aaa",
		})
	}

	var hostTools []TargetSpecHostTool
	for i := 0; i < factor; i++ {
		hostTools = append(hostTools, TargetSpecHostTool{
			Name: "aaa",
			Path: "/bin/aaa",
		})
	}

	var out []TargetSpecOutFile
	for i := 0; i < factor; i++ {
		out = append(out, TargetSpecOutFile{
			Name:    "",
			Package: nil,
			Path:    "",
		})
	}

	var cacheFiles []string
	for i := 0; i < factor; i++ {
		cacheFiles = append(cacheFiles, "aaa")
	}

	var cacheNames []string
	for i := 0; i < factor; i++ {
		cacheNames = append(cacheNames, "aaa")
	}

	var labels []string
	for i := 0; i < factor; i++ {
		labels = append(labels, "aaa")
	}

	var env = map[string]string{}
	for i := 0; i < factor; i++ {
		env[fmt.Sprintf("ENV%v", i)] = "aaa"
	}

	var passEnv []string
	for i := 0; i < factor; i++ {
		passEnv = append(passEnv, fmt.Sprintf("ENV%v", i))
	}

	return TargetSpec{
		Name:              name,
		FQN:               "//:" + name,
		Package:           pkg,
		Run:               []string{"some", "command"},
		Executor:          "exec",
		Quiet:             false,
		Dir:               "",
		PassArgs:          false,
		Deps:              deps,
		HashDeps:          deps,
		DifferentHashDeps: true,
		Tools: TargetSpecTools{
			Targets: targetTools,
			Hosts:   hostTools,
			Exprs:   exprTools,
		},
		Out: out,
		Cache: TargetSpecCache{
			Enabled: true,
			Named:   cacheNames,
			Files:   cacheFiles,
		},
		Sandbox:    false,
		Codegen:    "",
		Labels:     labels,
		Env:        env,
		PassEnv:    passEnv,
		RunInCwd:   false,
		Gen:        false,
		Source:     []string{"some_source" + time.Now().String()},
		RuntimeEnv: nil,
		SrcEnv:     "",
		OutEnv:     "",
		HashFile:   "",
	}
}

var equal bool

func TestTargetSpec_Equal(t *testing.T) {
	t.Parallel()

	t1 := genTargetSpec("aaa", 2)
	t2 := genTargetSpec("aaa", 2)

	assert.True(t, t1.Equal(t2))
}

func benchmarkTargetSpecEqual(b *testing.B, factor int, f func(t1, t2 TargetSpec) bool) {
	t1 := genTargetSpec("aaa", factor)
	t2 := genTargetSpec("aaa", factor)

	b.ResetTimer()
	var v bool
	for n := 0; n < b.N; n++ {
		v = f(t1, t2)
	}

	equal = v
}

func benchmarkTargetSpecEqualJson(b *testing.B, factor int) {
	benchmarkTargetSpecEqual(b, factor, func(t1, t2 TargetSpec) bool {
		return t1.equalJson(t2)
	})
}

func benchmarkTargetSpecEqualStruct(b *testing.B, factor int) {
	benchmarkTargetSpecEqual(b, factor, func(t1, t2 TargetSpec) bool {
		return t1.equalStruct(t2)
	})
}

func BenchmarkTargetSpec_EqualJson1(b *testing.B)    { benchmarkTargetSpecEqualJson(b, 1) }
func BenchmarkTargetSpec_EqualJson10(b *testing.B)   { benchmarkTargetSpecEqualJson(b, 10) }
func BenchmarkTargetSpec_EqualJson100(b *testing.B)  { benchmarkTargetSpecEqualJson(b, 100) }
func BenchmarkTargetSpec_EqualJson1000(b *testing.B) { benchmarkTargetSpecEqualJson(b, 1000) }

func BenchmarkTargetSpec_EqualStruct1(b *testing.B)    { benchmarkTargetSpecEqualStruct(b, 1) }
func BenchmarkTargetSpec_EqualStruct10(b *testing.B)   { benchmarkTargetSpecEqualStruct(b, 10) }
func BenchmarkTargetSpec_EqualStruct100(b *testing.B)  { benchmarkTargetSpecEqualStruct(b, 100) }
func BenchmarkTargetSpec_EqualStruct1000(b *testing.B) { benchmarkTargetSpecEqualStruct(b, 1000) }

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

			var spec TargetSpec

			e := &runBuildEngine{
				pkg: &Package{
					Name:     "test",
					FullName: "some/test",
					Root: Path{
						root:    "/tmp/some/test",
						relRoot: "some/test",
					},
				},
				registerTarget: func(rspec TargetSpec) error {
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
