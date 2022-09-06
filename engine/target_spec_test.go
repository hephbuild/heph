package engine

import (
	"fmt"
	"github.com/stretchr/testify/assert"
	"heph/utils"
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
			Name:    "aaa",
			Package: pkg,
			Path:    "aaa",
		})
		deps.Exprs = append(deps.Exprs, TargetSpecDepExpr{
			Name:    "aaa",
			Package: pkg,
			Expr: &utils.Expr{
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
			Expr: &utils.Expr{
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

	var cachedFiles []string
	for i := 0; i < factor; i++ {
		cachedFiles = append(cachedFiles, "aaa")
	}

	var labels []string
	for i := 0; i < factor; i++ {
		labels = append(labels, "aaa")
	}

	var env = map[string]string{}
	for i := 0; i < factor; i++ {
		env[fmt.Sprintf("ENV%v", i)] = "aaa"
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
		ExprTools:         exprTools,
		TargetTools:       targetTools,
		HostTools:         hostTools,
		Out:               out,
		ShouldCache:       false,
		CachedFiles:       cachedFiles,
		Sandbox:           false,
		Codegen:           "",
		Labels:            labels,
		Env:               env,
		PassEnv:           nil,
		RunInCwd:          false,
		Gen:               false,
		Source:            []string{"some_source" + time.Now().String()},
		RuntimeEnv:        nil,
		RequireGen:        false,
		SrcEnv:            "",
		OutEnv:            "",
		HashFile:          "",
	}
}

var equal bool

func TestTargetSpec_Equal(t *testing.T) {
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

func BenchmarkTargetSpec_EqualJson1(b *testing.B)    { benchmarkTargetSpecEqualJson(b, 1) }
func BenchmarkTargetSpec_EqualJson10(b *testing.B)   { benchmarkTargetSpecEqualJson(b, 10) }
func BenchmarkTargetSpec_EqualJson100(b *testing.B)  { benchmarkTargetSpecEqualJson(b, 100) }
func BenchmarkTargetSpec_EqualJson1000(b *testing.B) { benchmarkTargetSpecEqualJson(b, 1000) }
