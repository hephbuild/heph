package plugingo

import (
	"context"
	"fmt"
	"path/filepath"

	"github.com/hephbuild/heph/internal/htypes"

	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"google.golang.org/protobuf/types/known/structpb"
)

const unsafePkgName = "unsafe"

func (p *Plugin) packageBin(ctx context.Context, basePkg string, goPkg Package, factors Factors, requestId string) (*pluginv1.GetResponse, error) {
	c := p.newGetGoPackageCache(ctx, basePkg, factors, requestId)

	goPkgs, err := p.goImportsToDeps(ctx, goPkg.Imports, factors, c, requestId, nil)
	if err != nil {
		return nil, err
	}

	mainRef := tref.Format(tref.WithOut(goPkg.GetBuildLibTargetRef(ModeNormal), "a"))

	return p.packageBinInner(ctx, "build", goPkg, factors, mainRef, goPkgs)
}

func (p *Plugin) packageBinInner(
	ctx context.Context,
	targetName string,
	goPkg Package,
	factors Factors,
	mainRef string,
	depsGoPkgs []LibPackage,
) (*pluginv1.GetResponse, error) {
	deps := map[string][]string{}
	run := []string{
		`echo > importconfig`,
	}
	for i, depGoPkg := range depsGoPkgs {
		if depGoPkg.ImportPath == unsafePkgName {
			// ignore pseudo package
			continue
		}

		deps[fmt.Sprintf("lib%v", i)] = []string{tref.Format(tref.WithOut(depGoPkg.LibTargetRef, "a"))}

		run = append(run, fmt.Sprintf(`echo "packagefile %v=${SRC_LIB%v}" >> importconfig`, depGoPkg.ImportPath, i))
	}

	deps["main"] = []string{mainRef}

	run = append(run, `go tool link -importcfg "importconfig" -o $OUT $SRC_MAIN`)

	return pluginv1.GetResponse_builder{
		Spec: pluginv1.TargetSpec_builder{
			Ref: pluginv1.TargetRef_builder{
				Package: htypes.Ptr(goPkg.GetHephBuildPackage()),
				Name:    htypes.Ptr(targetName),
				Args:    factors.Args(),
			}.Build(),
			Driver: htypes.Ptr("bash"),
			Config: map[string]*structpb.Value{
				"env": p.getEnvStructpb(factors, map[string]string{
					"GO_EXTLINK_ENABLED": "0",
				}),
				"runtime_pass_env": p.getRuntimePassEnvStructpb(),
				"run":              hstructpb.NewStringsValue(run),
				"out":              structpb.NewStringValue(filepath.Base(goPkg.HephPackage)),
				"deps":             hstructpb.NewMapStringStringsValue(deps),
				"tools":            p.getGoToolStructpb(),
			},
			Labels: []string{"go-build"},
		}.Build(),
	}.Build(), nil
}
