package plugingo

import (
	"connectrpc.com/connect"
	"context"
	"fmt"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"google.golang.org/protobuf/types/known/structpb"
)

func (p *Plugin) goModDownload(ctx context.Context, pkg, goMod, version string) (*connect.Response[pluginv1.GetResponse], error) {
	run := []string{
		"echo module heph_ignore > go.mod", // stops go from reading the main go.mod, and downloading all of those too
		fmt.Sprintf("go mod download -modcacherw -json %v@%v | tee mod.json", goMod, version),
		"rm go.mod",
		`export MOD_DIR=$(cat mod.json | awk -F\" '/"Dir": / { print $4 }')`,
		`cp -r "$MOD_DIR/." .`,
	}

	return connect.NewResponse(&pluginv1.GetResponse{Spec: &pluginv1.TargetSpec{
		Ref: &pluginv1.TargetRef{
			Package: pkg,
			Name:    "download",
			Driver:  "sh",
		},
		Config: map[string]*structpb.Value{
			"env": hstructpb.NewMapStringStringValue(map[string]string{
				"CGO_ENABLED": "0",
				"GOWORK":      "off",
			}),
			"runtime_pass_env": hstructpb.NewStringsValue([]string{"HOME"}),
			"run":              hstructpb.NewStringsValue(run),
			"out":              structpb.NewStringValue("."),
			"cache":            structpb.NewBoolValue(true),
			// "tools": hstructpb.NewStringsValue([]string{fmt.Sprintf("//go_toolchain/%v:go", f.GoVersion)}),
		},
	}}), nil
}

func ThirdpartyContentPackage(goMod, version, goPath string) string {
	return tref.JoinPackage(ThirdpartyPrefix, goMod+"@"+version, goPath)
}

func ThirdpartyBuildPackage(basePkg, goMod, version, goPath string) string {
	return tref.JoinPackage(basePkg, ThirdpartyPrefix, goMod+"@"+version, goPath)
}

func (p *Plugin) goModContent(ctx context.Context, goMod, version, modPath string) (*connect.Response[pluginv1.GetResponse], error) {
	return connect.NewResponse(&pluginv1.GetResponse{Spec: &pluginv1.TargetSpec{
		Ref: &pluginv1.TargetRef{
			Package: ThirdpartyContentPackage(goMod, version, modPath),
			Name:    "content",
			Driver:  "sh",
		},
		Config: map[string]*structpb.Value{
			"out":   structpb.NewStringValue("."),
			"cache": structpb.NewBoolValue(true),
			"deps": structpb.NewStringValue(tref.Format(&pluginv1.TargetRef{
				Package: ThirdpartyContentPackage(goMod, version, ""),
				Name:    "download",
			})),
			// "tools": hstructpb.NewStringsValue([]string{fmt.Sprintf("//go_toolchain/%v:go", f.GoVersion)}),
		},
	}}), nil
}

func (p *Plugin) goModContentIn(ctx context.Context, basePkg, currentPkg string, goPkg Package, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	basePkg, modPath, version, modPkgPath, ok := ParseThirdpartyPackage(currentPkg)
	if !ok {
		return nil, fmt.Errorf("invalid package")
	}

	var allFiles []string
	allFiles = append(allFiles, goPkg.GoFiles...)
	allFiles = append(allFiles, goPkg.SFiles...)

	run := []string{}
	for _, file := range allFiles {
		run = append(run, fmt.Sprintf("mv $WORKDIR/%v/%v .", ThirdpartyContentPackage(modPath, version, modPkgPath), file))
	}

	return connect.NewResponse(&pluginv1.GetResponse{Spec: &pluginv1.TargetSpec{
		Ref: &pluginv1.TargetRef{
			Package: currentPkg,
			Name:    "content",
			Driver:  "sh",
			Args:    factors.Args(),
		},
		Config: map[string]*structpb.Value{
			"out":   structpb.NewStringValue("."),
			"cache": structpb.NewBoolValue(true),
			"run":   hstructpb.NewStringsValue(run),
			"deps": structpb.NewStringValue(tref.Format(&pluginv1.TargetRef{
				Package: ThirdpartyContentPackage(modPath, version, modPkgPath),
				Name:    "content",
			})),
			// "tools": hstructpb.NewStringsValue([]string{fmt.Sprintf("//go_toolchain/%v:go", f.GoVersion)}),
		},
	}}), nil
}
