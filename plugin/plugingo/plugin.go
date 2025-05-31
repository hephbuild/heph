package plugingo

import (
	"connectrpc.com/connect"
	"context"
	"errors"
	"fmt"
	cache "github.com/Code-Hex/go-generics-cache"
	"github.com/Code-Hex/go-generics-cache/policy/lru"
	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/internal/hsingleflight"
	"github.com/hephbuild/heph/lib/engine"
	engine2 "github.com/hephbuild/heph/lib/engine"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"google.golang.org/protobuf/types/known/structpb"
	"os"
	"path/filepath"
	"runtime"
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

var _ engine2.Provider = (*Plugin)(nil)
var _ engine2.PluginIniter = (*Plugin)(nil)

type packageCacheKey struct {
	RequestId string
	Factors   Factors
	BasePkg   string
}

type stdCacheKey struct {
	RequestId string
	Factors   Factors
}

type moduleCacheKey struct {
	RequestId string
	Factors   Factors
}

type Plugin struct {
	resultClient     engine.EngineHandle
	root             string
	resultStdListMem hsingleflight.GroupMem[[]Package]

	packageCache *cache.Cache[packageCacheKey, *GetGoPackageCache]
	moduleCache  *cache.Cache[moduleCacheKey, func() ([]Module, error)]
	stdCache     *cache.Cache[stdCacheKey, func() (map[string]Package, error)]
}

func (p *Plugin) PluginInit(ctx context.Context, init engine.PluginInit) error {
	p.resultClient = init.CoreHandle
	p.root = init.Root

	return nil
}

const Name = "go"

func New() *Plugin {
	return &Plugin{
		packageCache: cache.New(cache.AsLRU[packageCacheKey, *GetGoPackageCache](lru.WithCapacity(10000))),
		stdCache:     cache.New[stdCacheKey, func() (map[string]Package, error)](),
		moduleCache:  cache.New[moduleCacheKey, func() ([]Module, error)](),
	}
}

func (p *Plugin) Config(ctx context.Context, c *pluginv1.ProviderConfigRequest) (*pluginv1.ProviderConfigResponse, error) {
	return &pluginv1.ProviderConfigResponse{
		Name: Name,
	}, nil
}

func (p *Plugin) Probe(ctx context.Context, c *pluginv1.ProbeRequest) (*pluginv1.ProbeResponse, error) {
	return &pluginv1.ProbeResponse{}, nil
}

func (p *Plugin) List(ctx context.Context, req *pluginv1.ListRequest) (engine2.HandlerStreamReceive[*pluginv1.ListResponse], error) {
	return engine2.NewChanHandlerStreamFunc(func(send func(*pluginv1.ListResponse) error) error {
		if _, ok := tref.CutPackagePrefix(req.GetPackage(), "@heph/file"); ok {
			return nil
		}

		// TODO: factors matrix
		for _, factors := range []Factors{{
			GoVersion: "1.24",
			GOOS:      runtime.GOOS,
			GOARCH:    runtime.GOARCH,
		}} {
			if _, ok := tref.CutPackagePrefix(req.GetPackage(), "@heph/go/std"); ok {
				// TODO: std

				return nil
			}

			if tref.HasPackagePrefix(req.GetPackage(), "@heph/go/thirdparty") {
				// TODO

				return nil
			}

			goPkg, err := p.getGoPackageFromHephPackage(ctx, req.Package, factors, req.RequestId)
			if err != nil {
				if errors.Is(err, errNotInGoModule) {
					return nil
				}

				if strings.Contains(err.Error(), "no Go files") {
					return nil
				}

				return err
			}

			if len(goPkg.GoFiles) > 0 {
				err = send(&pluginv1.ListResponse{Of: &pluginv1.ListResponse_Ref{
					Ref: goPkg.GetBuildLibTargetRef(ModeNormal),
				}})
				if err != nil {
					return err
				}
			}

			if goPkg.IsCommand() {
				err = send(&pluginv1.ListResponse{Of: &pluginv1.ListResponse_Ref{
					Ref: &pluginv1.TargetRef{
						Package: goPkg.GetHephBuildPackage(),
						Name:    "build",
						Args:    factors.Args(),
					},
				}})
				if err != nil {
					return err
				}
			}

			if !goPkg.Is3rdParty && !goPkg.IsStd && (len(goPkg.TestGoFiles) > 0 || len(goPkg.XTestGoFiles) > 0) {
				err = send(&pluginv1.ListResponse{Of: &pluginv1.ListResponse_Ref{
					Ref: &pluginv1.TargetRef{
						Package: goPkg.GetHephBuildPackage(),
						Name:    "test",
						Args:    factors.Args(),
					},
				}})
				if err != nil {
					return err
				}
			}
		}

		return nil

	}), nil
}

const ThirdpartyPrefix = "@heph/go/thirdparty"

func getMode(ref *pluginv1.TargetRef) (string, error) {
	return ref.Args["mode"], nil
}

func (p *Plugin) Get(ctx context.Context, req *pluginv1.GetRequest) (_ *pluginv1.GetResponse, rerr error) {
	if tref.HasPackagePrefix(req.GetRef().GetPackage(), "@heph/file") {
		return nil, engine2.ErrNotFound
	}

	defer func() {
		if rerr != nil {
			rerr = connect.NewError(connect.CodeInternal, rerr)
		}
	}()

	factors := FactorsFromArgs(req.GetRef().GetArgs())
	if factors.GoVersion == "" {
		factors.GoVersion = "1.24"
	}
	if factors.GOOS == "" {
		factors.GOOS = runtime.GOOS
	}
	if factors.GOARCH == "" {
		factors.GOARCH = runtime.GOARCH
	}

	if basePkg, modPath, version, modPkgPath, ok := ParseThirdpartyPackage(req.GetRef().GetPackage()); ok && basePkg == "" {
		switch req.GetRef().GetName() {
		case "download":
			if modPkgPath != "" {
				return nil, fmt.Errorf("modpath is unsupported on download")
			}

			return p.goModDownload(ctx, req.GetRef().GetPackage(), modPath, version)
		}
	}

	if goImport, ok := tref.CutPackagePrefix(req.GetRef().GetPackage(), "@heph/go/std"); ok {
		if goImport == "" && req.GetRef().GetName() == "install" {
			return p.stdInstall(ctx, factors)
		}

		if req.GetRef().GetName() == "build_lib" {
			return p.stdLibBuild(ctx, factors, goImport)
		}

		return nil, engine2.ErrNotFound
	}

	switch req.GetRef().GetName() {
	case "build":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.RequestId)
		if err != nil {
			return nil, err
		}

		gomod, _, err := p.getGoModGoWork(ctx, req.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		return p.packageBin(ctx, tref.DirPackage(gomod), goPkg, factors, req.RequestId)
	case "test":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.RequestId)
		if err != nil {
			return nil, err
		}

		return p.runTest(ctx, goPkg, factors)
	case "embedcfg":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.RequestId)
		if err != nil {
			return nil, err
		}

		gomod, _, err := p.getGoModGoWork(ctx, req.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		mode, err := getMode(req.GetRef())
		if err != nil {
			return nil, fmt.Errorf("parse mode: %w", err)
		}

		return p.embedCfg(ctx, tref.DirPackage(gomod), req.GetRef().GetPackage(), goPkg, factors, mode)
	case "build_lib":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.RequestId)
		if err != nil {
			return nil, err
		}

		gomod, _, err := p.getGoModGoWork(ctx, req.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		mode, err := getMode(req.GetRef())
		if err != nil {
			return nil, fmt.Errorf("parse mode: %w", err)
		}

		return p.packageLib(ctx, tref.DirPackage(gomod), goPkg, factors, mode, req.RequestId)
	case "build_test":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.RequestId)
		if err != nil {
			return nil, err
		}

		gomod, _, err := p.getGoModGoWork(ctx, req.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		return p.packageBinTest(ctx, tref.DirPackage(gomod), goPkg, factors, req.RequestId)
	case "testmain":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.RequestId)
		if err != nil {
			return nil, err
		}

		return p.generateTestMain(ctx, goPkg, factors)
	case "build_testmain_lib":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.RequestId)
		if err != nil {
			return nil, err
		}

		gomod, _, err := p.getGoModGoWork(ctx, req.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		return p.testMainLib(ctx, tref.DirPackage(gomod), goPkg, factors, req.RequestId)
	case "build_lib#asm":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.RequestId)
		if err != nil {
			return nil, err
		}

		mode, err := getMode(req.GetRef())
		if err != nil {
			return nil, fmt.Errorf("parse mode: %w", err)
		}

		return p.packageLibAsm(ctx, goPkg, factors, req.GetRef().Args["file"], mode)
	case "build_lib#abi":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.RequestId)
		if err != nil {
			return nil, err
		}

		mode, err := getMode(req.GetRef())
		if err != nil {
			return nil, fmt.Errorf("parse mode: %w", err)
		}

		return p.packageLibAbi(ctx, goPkg, factors, mode)
	case "build_lib#incomplete":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.RequestId)
		if err != nil {
			return nil, err
		}

		gomod, _, err := p.getGoModGoWork(ctx, req.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		mode, err := getMode(req.GetRef())
		if err != nil {
			return nil, fmt.Errorf("parse mode: %w", err)
		}

		return p.packageLibIncomplete(ctx, tref.DirPackage(gomod), goPkg, factors, mode, req.RequestId)
	}

	return nil, engine2.ErrNotFound
}

func (p *Plugin) goListPkg(ctx context.Context, pkg string, f Factors, imp, requestId string) ([]*pluginv1.Artifact, *pluginv1.TargetRef, error) {
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

	args := f.Args()
	if imp != "." {
		args = hmaps.Concat(args, map[string]string{
			"import": imp,
		})
	}

	listRef := &pluginv1.TargetRef{
		Package: pkg,
		Name:    "_golist",
		Args:    args,
	}

	res, err := p.resultClient.ResultClient.Get(ctx, &corev1.ResultRequest{
		RequestId: requestId,
		Of: &corev1.ResultRequest_Spec{
			Spec: &pluginv1.TargetSpec{
				Ref:    listRef,
				Driver: "sh",
				Config: map[string]*structpb.Value{
					"env": hstructpb.NewMapStringStringValue(map[string]string{
						"GOOS":        f.GOOS,
						"GOARCH":      f.GOARCH,
						"CGO_ENABLED": "0",
					}),
					"runtime_pass_env": hstructpb.NewStringsValue([]string{"HOME"}),
					"run":              structpb.NewStringValue(fmt.Sprintf("go list -mod=readonly -json -tags %q %v > $OUT", f.Tags, imp)),
					"out":              structpb.NewStringValue("golist.json"),
					"in_tree":          structpb.NewBoolValue(true),
					"cache":            structpb.NewStringValue("local"),
					"hash_deps":        hstructpb.NewStringsValue(files),
					// "tools": hstructpb.NewStringsValue([]string{fmt.Sprintf("//go_toolchain/%v:go", f.GoVersion)}),
				},
			},
		},
	})
	if err != nil {
		return nil, nil, fmt.Errorf("golist: %v (in %v): %v: %w", imp, pkg, tref.Format(listRef), err)
	}

	return res.GetArtifacts(), res.GetDef().GetRef(), nil
}
