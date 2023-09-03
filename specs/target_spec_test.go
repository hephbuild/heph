package specs

import (
	"fmt"
	"github.com/hephbuild/heph/exprs"
	"github.com/hephbuild/heph/packages"
	"github.com/stretchr/testify/assert"
	"strings"
	"testing"
	"time"
)

func genTargetSpec(name string, factor int) Target {
	pkg := &packages.Package{
		Path: "aaa",
	}

	var deps Deps
	for i := 0; i < factor; i++ {
		deps.Targets = append(deps.Targets, DepTarget{
			Name:   "aaa",
			Output: "aaa",
			Target: "//aaa",
		})
		deps.Files = append(deps.Files, DepFile{
			Name: "aaa",
			Path: "aaa",
		})
		deps.Exprs = append(deps.Exprs, DepExpr{
			Name: "aaa",
			Expr: exprs.Expr{
				String:    "$(aaa)",
				Function:  "aaa",
				PosArgs:   nil,
				NamedArgs: nil,
			},
		})
	}

	var exprTools []ExprTool
	for i := 0; i < factor; i++ {
		exprTools = append(exprTools, ExprTool{
			Expr: exprs.Expr{
				String:    "$(aaa)",
				Function:  "aaa",
				PosArgs:   nil,
				NamedArgs: nil,
			},
			Output: "",
		})
	}

	var targetTools []TargetTool
	for i := 0; i < factor; i++ {
		targetTools = append(targetTools, TargetTool{
			Target: "//:aaa",
			Output: "aaa",
		})
	}

	var hostTools []HostTool
	for i := 0; i < factor; i++ {
		hostTools = append(hostTools, HostTool{
			Name: "aaa",
			Path: "/bin/aaa",
		})
	}

	var out []OutFile
	for i := 0; i < factor; i++ {
		out = append(out, OutFile{
			Name: "",
			Path: "",
		})
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

	return Target{
		Name:              name,
		Addr:              "//:" + name,
		Package:           pkg,
		Run:               []string{"some", "command"},
		Entrypoint:        "exec",
		Dir:               "",
		PassArgs:          false,
		Deps:              deps,
		HashDeps:          deps,
		DifferentHashDeps: true,
		Tools: Tools{
			Targets: targetTools,
			Hosts:   hostTools,
			Exprs:   exprTools,
		},
		Out: out,
		Cache: Cache{
			Enabled: true,
			Named:   cacheNames,
		},
		Sandbox:    false,
		Codegen:    "",
		Labels:     labels,
		Env:        env,
		PassEnv:    passEnv,
		RunInCwd:   false,
		Gen:        false,
		Source:     []Source{{Name: "some_source" + time.Now().String()}},
		RuntimeEnv: nil,
		SrcEnv:     SrcEnv{},
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

func benchmarkTargetSpecEqual(b *testing.B, factor int, f func(t1, t2 Target) bool) {
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
	benchmarkTargetSpecEqual(b, factor, func(t1, t2 Target) bool {
		return t1.equalJson(t2)
	})
}

func benchmarkTargetSpecEqualStruct(b *testing.B, factor int) {
	benchmarkTargetSpecEqual(b, factor, func(t1, t2 Target) bool {
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

func TestSortOutputsForHashing(t *testing.T) {
	tests := []struct {
		outputs  []string
		expected []string
	}{
		{[]string{"a", "c", "b"}, []string{"a", "b", "c"}},
		{[]string{SupportFilesOutput, "b"}, []string{SupportFilesOutput, "b"}},
		{[]string{"a", "b", SupportFilesOutput}, []string{SupportFilesOutput, "a", "b"}},
		{[]string{"a", "b", SupportFilesOutput, "c"}, []string{SupportFilesOutput, "a", "b", "c"}},
	}

	for _, test := range tests {
		t.Run(strings.Join(test.outputs, ","), func(t *testing.T) {
			actual := SortOutputsForHashing(test.outputs)

			assert.EqualValues(t, test.expected, actual)
		})
	}
}
