package plugingo

import (
	"connectrpc.com/connect"
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/internal/hsingleflight"
	"github.com/hephbuild/heph/lib/engine"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
	"github.com/hephbuild/heph/plugin/tref"
	"google.golang.org/protobuf/types/known/structpb"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
)

type Factors struct {
	GoVersion    string
	GOOS, GOARCH string
	Tags         string
}

func (f Factors) Args() map[string]string {
	if len(f.Tags) == 0 {
		return map[string]string{
			"os":   f.GOOS,
			"arch": f.GOARCH,
		}
	}

	return map[string]string{
		"os":   f.GOOS,
		"arch": f.GOARCH,
		"tags": f.Tags,
	}
}

func FactorsFromArgs(args map[string]string) Factors {
	return Factors{
		GOOS:   args["os"],
		GOARCH: args["arch"],
		Tags:   args["tags"],
	}
}

var _ pluginv1connect.ProviderHandler = (*Plugin)(nil)
var _ engine.PluginIniter = (*Plugin)(nil)

type Plugin struct {
	resultClient     engine.EngineHandle
	root             string
	resultStdListMem hsingleflight.GroupMem[[]Package]
}

func (p *Plugin) PluginInit(ctx context.Context, init engine.PluginInit) error {
	p.resultClient = init.CoreHandle
	p.root = init.Root

	return nil
}

func New() *Plugin {
	return &Plugin{}
}

func (p *Plugin) Config(ctx context.Context, c *connect.Request[pluginv1.ProviderConfigRequest]) (*connect.Response[pluginv1.ProviderConfigResponse], error) {
	return connect.NewResponse(&pluginv1.ProviderConfigResponse{
		Name: "go",
	}), nil
}

func (p *Plugin) Probe(ctx context.Context, c *connect.Request[pluginv1.ProbeRequest]) (*connect.Response[pluginv1.ProbeResponse], error) {
	return connect.NewResponse(&pluginv1.ProbeResponse{}), nil
}

func (p *Plugin) GetSpecs(ctx context.Context, req *connect.Request[pluginv1.GetSpecsRequest], res *connect.ServerStream[pluginv1.GetSpecsResponse]) error {
	return connect.NewError(connect.CodeUnimplemented, errors.New("not implemented"))
}

func (p *Plugin) List(ctx context.Context, req *connect.Request[pluginv1.ListRequest], res *connect.ServerStream[pluginv1.ListResponse]) error {
	// artifacts, ref, err := p.goListPkg(ctx, req.Msg.Package, Factors{}) // for all factors
	// hlog.From(ctx).With(slog.Any("artifacts", artifacts), slog.Any("ref", ref), slog.Any("err", err)).Warn("LIST")

	return nil
}

const ThirdpartyPrefix = "@heph/go/thirdparty"

func (p *Plugin) Get(ctx context.Context, req *connect.Request[pluginv1.GetRequest]) (*connect.Response[pluginv1.GetResponse], error) {
	if tref.HasPackagePrefix(req.Msg.GetRef().GetPackage(), "@heph/file") {
		return nil, connect.NewError(connect.CodeNotFound, errors.New("not found"))
	}

	factors := FactorsFromArgs(req.Msg.GetRef().GetArgs())
	if factors.GoVersion == "" {
		factors.GoVersion = "1.24"
	}
	if factors.GOOS == "" {
		factors.GOOS = runtime.GOOS
	}
	if factors.GOARCH == "" {
		factors.GOARCH = runtime.GOARCH
	}

	if basePkg, modPath, version, modPkgPath, ok := ParseThirdpartyPackage(req.Msg.GetRef().GetPackage()); ok && basePkg == "" {
		switch req.Msg.GetRef().GetName() {
		case "download":
			if modPkgPath != "" {
				return nil, fmt.Errorf("modpath is unsupported on download")
			}

			return p.goModDownload(ctx, req.Msg.GetRef().GetPackage(), modPath, version)
		case "content":
			return p.goModContent(ctx, modPath, version, modPkgPath, req.Msg.GetRef().GetPackage(), req.Msg.GetRef().Args["f"])
		}
	}

	if goImport, ok := tref.CutPackagePrefix(req.Msg.GetRef().GetPackage(), "@heph/go/std"); ok {
		if goImport == "" && req.Msg.GetRef().GetName() == "install" {
			return p.stdInstall(ctx, factors)
		}

		if req.Msg.GetRef().GetName() == "build_lib" {
			return p.stdLibBuild(ctx, factors, goImport)
		}

		return nil, connect.NewError(connect.CodeNotFound, errors.New("not found"))
	}

	switch req.Msg.GetRef().GetName() {
	case "build":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.Msg.GetRef().GetPackage(), factors)
		if err != nil {
			return nil, err
		}

		gomod, _, err := p.getGoModGoWork(ctx, req.Msg.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		return p.packageBin(ctx, tref.DirPackage(gomod), goPkg, factors)
	case "test":
		return p.runTest(ctx, req.Msg.GetRef().GetPackage(), factors)
	case "embedcfg":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.Msg.GetRef().GetPackage(), factors)
		if err != nil {
			return nil, err
		}

		gomod, _, err := p.getGoModGoWork(ctx, req.Msg.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		return p.embedCfg(ctx, tref.DirPackage(gomod), req.Msg.GetRef().GetPackage(), goPkg, factors)
	case "build_lib":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.Msg.GetRef().GetPackage(), factors)
		if err != nil {
			return nil, err
		}

		gomod, _, err := p.getGoModGoWork(ctx, req.Msg.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		return p.packageLib(ctx, tref.DirPackage(gomod), goPkg, factors)
	case "build_test":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.Msg.GetRef().GetPackage(), factors)
		if err != nil {
			return nil, err
		}

		gomod, _, err := p.getGoModGoWork(ctx, req.Msg.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		return p.packageBinTest(ctx, tref.DirPackage(gomod), goPkg, factors)
	case "build_test_lib":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.Msg.GetRef().GetPackage(), factors)
		if err != nil {
			return nil, err
		}

		gomod, _, err := p.getGoModGoWork(ctx, req.Msg.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		x, err := strconv.ParseBool(req.Msg.GetRef().Args["x"])
		if err != nil {
			return nil, fmt.Errorf("parse x: %w", err)
		}

		return p.packageTestLib(ctx, tref.DirPackage(gomod), goPkg, factors, x)
	case "testmain":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.Msg.GetRef().GetPackage(), factors)
		if err != nil {
			return nil, err
		}

		return p.generateTestMain(ctx, goPkg, factors)
	case "build_testmain_lib":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.Msg.GetRef().GetPackage(), factors)
		if err != nil {
			return nil, err
		}

		gomod, _, err := p.getGoModGoWork(ctx, req.Msg.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		return p.testMainLib(ctx, tref.DirPackage(gomod), goPkg, factors)
	case "build_lib#asm":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.Msg.GetRef().GetPackage(), factors)
		if err != nil {
			return nil, err
		}

		return p.packageLibAsm(ctx, goPkg, factors, req.Msg.GetRef().Args["file"])
	case "build_lib#abi":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.Msg.GetRef().GetPackage(), factors)
		if err != nil {
			return nil, err
		}

		return p.packageLibAbi(ctx, goPkg, factors)
	case "build_lib#incomplete":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.Msg.GetRef().GetPackage(), factors)
		if err != nil {
			return nil, err
		}

		gomod, _, err := p.getGoModGoWork(ctx, req.Msg.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		return p.packageLibIncomplete(ctx, tref.DirPackage(gomod), goPkg, factors)
	}

	return nil, connect.NewError(connect.CodeNotFound, errors.New("not found"))
}

func (p *Plugin) goListPkg(ctx context.Context, pkg string, f Factors, imp string) ([]*pluginv1.Artifact, *pluginv1.TargetRef, error) {
	entries, err := os.ReadDir(filepath.Join(p.root, pkg))
	if err != nil {
		return nil, nil, err
	}

	var files []string
	for _, e := range entries {
		if e.IsDir() {
			continue
		}
		files = append(files, tref.FormatFile(pkg, e.Name()))
	}

	var args map[string]string
	if imp != "." {
		args = map[string]string{
			"import": imp,
		}
	}

	res, err := p.resultClient.ResultClient.Get(ctx, connect.NewRequest(&corev1.ResultRequest{
		Of: &corev1.ResultRequest_Spec{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: pkg,
					Name:    "_golist",
					Args:    args,
				},
				Driver: "sh",
				Config: map[string]*structpb.Value{
					"env": hstructpb.NewMapStringStringValue(map[string]string{
						"GODEBUG":     "installgoroot=all",
						"GOOS":        f.GOOS,
						"GOARCH":      f.GOARCH,
						"CGO_ENABLED": "0",
					}),
					"runtime_pass_env": hstructpb.NewStringsValue([]string{"HOME"}),
					"run":              structpb.NewStringValue(fmt.Sprintf("go list -mod=readonly -json -tags %q %v > $OUT", f.Tags, imp)),
					"out":              structpb.NewStringValue("golist.json"),
					"in_tree":          structpb.NewBoolValue(true),
					"cache":            structpb.NewBoolValue(true),
					"hash_deps":        hstructpb.NewStringsValue(files),
					// "tools": hstructpb.NewStringsValue([]string{fmt.Sprintf("//go_toolchain/%v:go", f.GoVersion)}),
				},
			},
		},
	}))
	if err != nil {
		return nil, nil, err
	}

	return res.Msg.GetArtifacts(), res.Msg.GetDef().GetRef(), nil
}
