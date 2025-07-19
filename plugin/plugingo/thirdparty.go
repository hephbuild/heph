package plugingo

import (
	"context"
	"fmt"

	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"google.golang.org/protobuf/types/known/structpb"
)

func (p *Plugin) goModDownload(ctx context.Context, pkg, goMod, version string) (*pluginv1.GetResponse, error) {
	run := []string{
		"echo module heph_ignore > go.mod", // stops go from reading the main go.mod, and downloading all of those too
		fmt.Sprintf("go mod download -modcacherw -json %v@%v | tee mod.json", goMod, version),
		"rm go.mod",
		`export MOD_DIR=$(cat mod.json | awk -F\" '/"Dir": / { print $4 }')`,
		`cp -r "$MOD_DIR/." .`,
	}

	return &pluginv1.GetResponse{Spec: &pluginv1.TargetSpec{
		Ref: &pluginv1.TargetRef{
			Package: pkg,
			Name:    "download",
		},
		Driver: "sh",
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
	}}, nil
}

func ThirdpartyContentPackage(goMod, version, goPath string) string {
	return tref.JoinPackage(ThirdpartyPrefix, goMod+"@"+version, goPath)
}

func ThirdpartyBuildPackage(basePkg, goMod, version, goPath string) string {
	return tref.JoinPackage(basePkg, ThirdpartyPrefix, goMod+"@"+version, goPath)
}
