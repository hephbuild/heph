package plugingo

import (
	"context"
	"fmt"
	"path"

	"connectrpc.com/connect"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"golang.org/x/sync/errgroup"
	"google.golang.org/protobuf/types/known/structpb"
)

func (p *Plugin) packageLib(ctx context.Context, goPkg Package, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	return p.packageLibInner(ctx, goPkg, factors)
}

func (p *Plugin) importPathToPackage(ctx context.Context, imp string) (string, error) {
	return imp, nil
}

func (p *Plugin) packageLibInner(ctx context.Context, goPkg Package, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	deps := map[string][]string{}
	run := []string{
		`echo > importconfig`,
	}
	imports := make([]Package, len(goPkg.Imports))
	var g errgroup.Group
	for i, imp := range goPkg.Imports {
		g.Go(func() error {
			impGoPkg, err := p.goFindPkg(ctx, goPkg.HephPackage, imp, factors)
			if err != nil {
				return err
			}

			imports[i] = impGoPkg

			return nil
		})
	}

	err := g.Wait()
	if err != nil {
		return nil, err
	}

	for i, imp := range goPkg.Imports {
		impGoPkg := imports[i]

		if impGoPkg.ImportPath == "unsafe" {
			// ignore pseudo package
			continue
		}

		deps[fmt.Sprintf("lib%v", i)] = []string{tref.Format(tref.WithOut(&pluginv1.TargetRef{
			Package: impGoPkg.HephPackage,
			Name:    "build_lib",
			Args:    factors.Args(),
		}, "a"))}

		run = append(run, fmt.Sprintf(`echo "packagefile %v=${SRC_LIB%v}" >> importconfig`, imp, i))
	}

	var extra string
	// if abi != "" { // if (asm pure)
	//	extra += " -symabis $SRC_ABI -asmhdr $SRC_ABI_H"
	//}

	if len(goPkg.EmbedPatterns) > 0 {
		extra += " -embedcfg $SRC_EMBED"
	}

	if true { // if !(asm pure)
		extra += " -complete"
	}

	for _, file := range goPkg.GoFiles {
		deps["src"] = append(deps["src"], tref.Format(&pluginv1.TargetRef{
			Package: path.Join("@heph/file", goPkg.HephPackage, file),
			Name:    ":content",
		}))
	}

	importPath := goPkg.ImportPath
	if goPkg.IsCommand() {
		importPath = MainPackage
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

// func (p *Plugin) packageLibPure(ctx context.Context, goPkg Package) (*connect.Response[pluginv1.GetResponse], error) {
//	goPkg.SFiles = nil
//
//	return p.packageLib(ctx, goPkg)
//}
