package plugingo

import (
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/plugin/tref"
	"os"
	"path"
	"path/filepath"
	"strconv"
	"strings"

	"connectrpc.com/connect"
	"github.com/hephbuild/heph/internal/hinstance"
	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/lib/engine"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
	"google.golang.org/protobuf/types/known/structpb"
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
	root         string
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

	if strings.HasPrefix(req.Msg.GetRef().GetPackage(), "@heph/go") {
		if req.Msg.GetRef().GetPackage() == "@heph/go/std" && req.Msg.GetRef().GetName() == "install" {
			return p.stdInstall(ctx, factors)
		}

		if req.Msg.GetRef().GetName() == "build_lib" {
			goImport := strings.TrimPrefix(req.Msg.GetRef().GetPackage(), "@heph/go/std/")

			return p.stdLibBuild(ctx, factors, goImport)
		}

		return nil, connect.NewError(connect.CodeNotFound, errors.New("not found"))
	}

	switch req.Msg.GetRef().GetName() {
	case "build":
		goPkg, err := p.goListPkgResult(ctx, req.Msg.GetRef().GetPackage(), factors)
		if err != nil {
			return nil, err
		}

		return p.packageBin(ctx, goPkg, factors)
	case "build_lib":
		goPkg, err := p.goListPkgResult(ctx, req.Msg.GetRef().GetPackage(), factors)
		if err != nil {
			return nil, err
		}

		return p.packageLib(ctx, goPkg, factors)
	case "build_lib#asm":
		// return p.packageLibAsm(ctx, goPkg, factors)
	case "build_lib#abi":
		// return p.packageLibAbi(ctx, goPkg, factors)
	case "build_lib#pure":
		// return p.packageLibAsmPure(ctx, goPkg, factors)
	case "build_lib#embed":
		// return p.packageLibEmbed(ctx, goPkg, factors)
	}

	return nil, connect.NewError(connect.CodeNotFound, errors.New("not found"))
}

func (p *Plugin) goListPkg(ctx context.Context, pkg string, f Factors, deps, find bool, imp string) ([]*pluginv1.Artifact, *pluginv1.TargetRef, error) {
	var extra string
	if deps {
		extra += " -deps"
	}

	if find {
		extra += " -find"
	}

	entries, err := os.ReadDir(filepath.Join(p.root, pkg))
	if err != nil {
		return nil, nil, err
	}

	var files []string
	for _, e := range entries {
		if e.IsDir() {
			continue
		}

		files = append(files, tref.Format(&pluginv1.TargetRef{
			Package: path.Join("@heph/file", pkg, e.Name()),
			Name:    "content",
		}))
	}

	res, err := p.resultClient.ResultClient.Get(ctx, connect.NewRequest(&corev1.ResultRequest{
		Of: &corev1.ResultRequest_Spec{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: pkg,
					Name:    "_golist",
					Driver:  "sh",
					Args: hmaps.Concat(f.Args(), map[string]string{
						"deps": strconv.FormatBool(deps),
						"find": strconv.FormatBool(find),
					}),
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
					"run":              structpb.NewStringValue(fmt.Sprintf("go list -mod=readonly -json -tags %q %v %v > $OUT", f.Tags, extra, imp)),
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
