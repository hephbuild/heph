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
)

func (p *Plugin) packageBin(ctx context.Context, goPkg Package, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	goPkgs, err := p.goListDepsPkgResult(ctx, goPkg.HephPackage, factors)
	if err != nil {
		return nil, err
	}

	deps := map[string][]string{}
	run := []string{
		`echo > importconfig`,
	}
	for i, goPkg := range goPkgs {
		if goPkg.IsStd {
			if goPkg.ImportPath == "unsafe" {
				// ignore pseudo package
				continue
			}

			deps[fmt.Sprintf("lib%v", i)] = []string{tref.Format(tref.WithOut(&pluginv1.TargetRef{
				Package: path.Join("@heph/go/std", goPkg.ImportPath),
				Name:    "build_lib",
				Args:    factors.Args(),
			}, "a"))}
		} else {
			if goPkg.IsCommand() {
				continue
			}

			deps[fmt.Sprintf("lib%v", i)] = []string{tref.Format(tref.WithOut(&pluginv1.TargetRef{
				Package: goPkg.HephPackage,
				Name:    "build_lib",
				Args:    factors.Args(),
			}, "a"))}
		}

		run = append(run, fmt.Sprintf(`echo "packagefile %v=${SRC_LIB%v}" >> importconfig`, goPkg.ImportPath, i))
	}

	deps["main"] = []string{tref.Format(tref.WithOut(&pluginv1.TargetRef{
		Package: goPkg.HephPackage,
		Name:    "build_lib",
		Args:    factors.Args(),
	}, "a"))}

	run = append(run, fmt.Sprintf(`go tool link -importcfg "importconfig" -o $OUT $SRC_MAIN`))

	return connect.NewResponse(&pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: goPkg.HephPackage,
				Name:    "build",
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
				"out":              structpb.NewStringValue(filepath.Base(goPkg.HephPackage)),
				"deps":             hstructpb.NewMapStringStringsValue(deps),
			},
		},
	}), nil
}
