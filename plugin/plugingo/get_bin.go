package plugingo

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"google.golang.org/protobuf/types/known/structpb"
	"path/filepath"
)

func (p *Plugin) packageBin(ctx context.Context, basePkg string, goPkg Package, factors Factors, requestId string) (*pluginv1.GetResponse, error) {
	c := p.newGetGoPackageCache(ctx, basePkg, factors, requestId)

	goPkgs, err := p.goImportsToDeps(ctx, goPkg.Imports, factors, c, requestId, nil)
	if err != nil {
		return nil, err
	}

	mainRef := tref.Format(tref.WithOut(goPkg.GetBuildLibTargetRef(ModeNormal), "a"))

	return p.packageBinInner(ctx, "build", goPkg, factors, mainRef, goPkgs)
}

func (p *Plugin) packageBinInner(ctx context.Context, targetName string, goPkg Package, factors Factors, mainRef string, goPkgs []LibPackage) (*pluginv1.GetResponse, error) {
	deps := map[string][]string{}
	run := []string{
		`echo > importconfig`,
	}
	for i, goPkg := range goPkgs {
		if goPkg.ImportPath == "unsafe" {
			// ignore pseudo package
			continue
		}

		deps[fmt.Sprintf("lib%v", i)] = []string{tref.Format(tref.WithOut(goPkg.LibTargetRef, "a"))}

		run = append(run, fmt.Sprintf(`echo "packagefile %v=${SRC_LIB%v}" >> importconfig`, goPkg.ImportPath, i))
	}

	deps["main"] = []string{mainRef}

	run = append(run, `go tool link -importcfg "importconfig" -o $OUT $SRC_MAIN`)

	return &pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: goPkg.GetHephBuildPackage(),
				Name:    targetName,
				Args:    factors.Args(),
			},
			Driver: "bash",
			Config: map[string]*structpb.Value{
				"env": hstructpb.NewMapStringStringValue(map[string]string{
					"GOOS":               factors.GOOS,
					"GOARCH":             factors.GOARCH,
					"CGO_ENABLED":        "0",
					"GO_EXTLINK_ENABLED": "0",
				}),
				"runtime_pass_env": hstructpb.NewStringsValue([]string{"HOME"}),
				"run":              hstructpb.NewStringsValue(run),
				"out":              structpb.NewStringValue(filepath.Base(goPkg.HephPackage)),
				"deps":             hstructpb.NewMapStringStringsValue(deps),
			},
			Labels: []string{"go-build"},
		},
	}, nil
}
