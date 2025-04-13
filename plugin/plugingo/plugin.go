package plugingo

import (
	"connectrpc.com/connect"
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/internal/hinstance"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/lib/engine"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
	"google.golang.org/protobuf/types/known/structpb"
	"strings"
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
	resultClient engine.EngineHandle
}

func (p *Plugin) PluginInit(ctx context.Context, init engine.PluginInit) error {
	p.resultClient = init.CoreHandle

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
	//artifacts, ref, err := p.goListPkg(ctx, req.Msg.Package, Factors{}) // for all factors
	//hlog.From(ctx).With(slog.Any("artifacts", artifacts), slog.Any("ref", ref), slog.Any("err", err)).Warn("LIST")

	return nil
}

func (p *Plugin) Get(ctx context.Context, req *connect.Request[pluginv1.GetRequest]) (*connect.Response[pluginv1.GetResponse], error) {
	factors := FactorsFromArgs(req.Msg.GetRef().GetArgs())
	if factors.GoVersion == "" {
		factors.GoVersion = "1.24"
	}
	if factors.GOOS == "" {
		factors.GOOS = "linux"
	}
	if factors.GOARCH == "" {
		factors.GOARCH = "amd64"
	}

	if strings.HasPrefix(req.Msg.GetRef().Package, "@heph/go") {
		if req.Msg.GetRef().Package == "@heph/go/std" && req.Msg.GetRef().Name == "install" {
			return p.stdInstall(ctx, factors)
		}

		if req.Msg.GetRef().Name == "build_lib" {
			goImport := strings.TrimPrefix(req.Msg.GetRef().Package, "@heph/go/std/")

			return p.stdLibBuild(ctx, factors, goImport)
		}

		return nil, connect.NewError(connect.CodeNotFound, errors.New("not found"))
	}

	switch req.Msg.GetRef().Name {
	case "build":
		goPkg, err := p.goListPkgResult(ctx, req.Msg.Ref.Package, factors)
		if err != nil {
			return nil, err
		}

		return p.packageBin(ctx, goPkg, factors)
	case "build_lib":
		goPkg, err := p.goListPkgResult(ctx, req.Msg.Ref.Package, factors)
		if err != nil {
			return nil, err
		}

		return p.packageLib(ctx, goPkg, factors)
	case "build_lib#asm":
		//return p.packageLibAsm(ctx, goPkg, factors)
	case "build_lib#abi":
		//return p.packageLibAbi(ctx, goPkg, factors)
	case "build_lib#pure":
		//return p.packageLibAsmPure(ctx, goPkg, factors)
	case "build_lib#embed":
		//return p.packageLibEmbed(ctx, goPkg, factors)
	}

	return nil, connect.NewError(connect.CodeNotFound, errors.New("not found"))
}

func (p *Plugin) goListPkg(ctx context.Context, pkg string, f Factors) ([]*pluginv1.Artifact, *pluginv1.TargetRef, error) {
	res, err := p.resultClient.ResultClient.Get(ctx, connect.NewRequest(&corev1.ResultRequest{
		Of: &corev1.ResultRequest_Spec{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: pkg,
					Name:    "_golist",
					Driver:  "sh",
					Args:    f.Args(),
				},
				Config: map[string]*structpb.Value{
					"env": hstructpb.NewMapStringStringValue(map[string]string{
						"GODEBUG":     "installgoroot=all",
						"GOOS":        f.GOOS,
						"GOARCH":      f.GOARCH,
						"CGO_ENABLED": "0",
						"HEPH_HASH":   hinstance.Hash(),
					}),
					"runtime_pass_env": hstructpb.NewStringsValue([]string{"HOME"}),
					"run":              structpb.NewStringValue(fmt.Sprintf("go list -mod=readonly -json -tags %q . > $OUT", f.Tags)),
					"out":              structpb.NewStringValue("golist.json"),
					"in_tree":          structpb.NewBoolValue(true),
					"cache":            structpb.NewBoolValue(false),
					//"tools": hstructpb.NewStringsValue([]string{fmt.Sprintf("//go_toolchain/%v:go", f.GoVersion)}),
					// TODO: cache based on go.mod
				},
			},
		},
	}))
	if err != nil {
		return nil, nil, err
	}

	return res.Msg.GetArtifacts(), res.Msg.Def.Ref, nil
}
