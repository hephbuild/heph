package plugingo

import (
	"connectrpc.com/connect"
	"context"
	"fmt"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"google.golang.org/protobuf/types/known/structpb"
	"path"
	"path/filepath"
	"slices"
)

func (p *Plugin) packageLib(ctx context.Context, goPkg Package, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	return p.packageLibInner(ctx, goPkg, factors)
}

func (p *Plugin) packageLibInner(ctx context.Context, goPkg Package, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	stdList, err := p.resultStdList(ctx, factors)
	if err != nil {
		return nil, fmt.Errorf("get stdlib list: %w", err)
	}

	deps := map[string][]string{}
	run := []string{
		`echo > importconfig`,
	}
	for i, imp := range goPkg.Imports {
		isStd := slices.ContainsFunc(stdList, func(p Package) bool {
			return p.ImportPath == imp
		})

		if isStd {
			deps[fmt.Sprintf("lib%v", i)] = []string{tref.Format(tref.WithOut(&pluginv1.TargetRef{
				Package: path.Join("@heph/go/std", imp),
				Name:    "build_lib",
				Args:    factors.Args(),
			}, "a"))}
		} else {
			deps[fmt.Sprintf("lib%v", i)] = []string{tref.Format(tref.WithOut(&pluginv1.TargetRef{
				Package: imp,
				Name:    "build_lib",
				Args:    factors.Args(),
			}, "a"))}
		}

		run = append(run, fmt.Sprintf(`echo "packagefile %v=${SRC_LIB%v}" >> importconfig`, imp, i))
	}

	var extra string
	//if abi != "" { // if (asm pure)
	//	extra += " -symabis $SRC_ABI -asmhdr $SRC_ABI_H"
	//}

	if len(goPkg.EmbedPatterns) > 0 {
		extra += " -embedcfg $SRC_EMBED"
	}

	if true { // if !(asm pure)
		extra += " -complete"
	}

	for _, file := range goPkg.GoFiles {
		deps["src"] = append(deps["src"], "//"+filepath.Join("@heph/file", goPkg.HephPackage, file)+":content")
	}

	importPath := goPkg.ImportPath
	if goPkg.Name == "main" {
		importPath = "main"
	}

	run = append(run, fmt.Sprintf(`go tool compile -importcfg importconfig -o $OUT_A -pack -p %v %v $SRC_SRC`, importPath, extra))

	return connect.NewResponse(&pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: goPkg.HephPackage,
				Name:    "build_lib",
				Driver:  "bash",
				Args:    factors.Args(),
			},
			Config: map[string]*structpb.Value{
				"env": hstructpb.NewMapStringStringValue(map[string]string{
					"GOOS":        factors.GOOS,
					"GOARCH":      factors.GOARCH,
					"CGO_ENABLED": "0",
				}),
				"runtime_pass_env": hstructpb.NewStringsValue([]string{"HOME"}),
				"run":              hstructpb.NewStringsValue(run),
				"out": hstructpb.NewMapStringStringValue(map[string]string{
					"a": goPkg.Name + ".a",
				}),
				"deps": hstructpb.NewMapStringStringsValue(deps),
			},
		},
	}), nil
}

//func (p *Plugin) packageLibPure(ctx context.Context, goPkg Package) (*connect.Response[pluginv1.GetResponse], error) {
//	goPkg.SFiles = nil
//
//	return p.packageLib(ctx, goPkg)
//}
