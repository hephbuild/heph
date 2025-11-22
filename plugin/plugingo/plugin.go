package plugingo

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"

	"github.com/hephbuild/heph/internal/htypes"

	"github.com/hephbuild/heph/lib/tref"

	cache "github.com/Code-Hex/go-generics-cache"
	"github.com/Code-Hex/go-generics-cache/policy/lru"
	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/internal/hsingleflight"
	"github.com/hephbuild/heph/lib/pluginsdk"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
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

var _ pluginsdk.Provider = (*Plugin)(nil)
var _ pluginsdk.Initer = (*Plugin)(nil)

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
	BasePkg   string
}

type goModGoWorkCache struct {
	gomod, gowork string
}

type Plugin struct {
	goTool           string
	resultClient     pluginsdk.Engine
	root             string
	resultStdListMem hsingleflight.GroupMem[Factors, []Package]

	packageCache     *cache.Cache[packageCacheKey, *GetGoPackageCache]
	moduleCache      hsingleflight.GroupMem[moduleCacheKey, []Module]
	stdCache         hsingleflight.GroupMem[stdCacheKey, map[string]Package]
	goModGoWorkCache hsingleflight.GroupMem[string, goModGoWorkCache]
}

func (p *Plugin) getGoToolStructpb() *structpb.Value {
	if p.goTool != "" {
		return structpb.NewStringValue(p.goTool)
	}

	return nil
}

func (p *Plugin) getRuntimePassEnvStructpb() *structpb.Value {
	return hstructpb.NewStringsValue([]string{"HOME"})
}

func (p *Plugin) getEnvStructpb2(ms ...map[string]string) *structpb.Value {
	m := map[string]string{
		"CGO_ENABLED": "0",
		"GOWORK":      "off",
		"GOTOOLCHAIN": "local",
	}
	if len(ms) == 0 {
		return hstructpb.NewMapStringStringValue(m)
	}

	return hstructpb.NewMapStringStringValue(hmaps.Concat(append([]map[string]string{m}, ms...)...))
}

func (p *Plugin) getEnvStructpb(factors Factors, ms ...map[string]string) *structpb.Value {
	m := map[string]string{
		"GOOS":        factors.GOOS,
		"GOARCH":      factors.GOARCH,
		"CGO_ENABLED": "0",
		"GOTOOLCHAIN": "local",
		"GOWORK":      "off",
	}

	if len(ms) == 0 {
		return hstructpb.NewMapStringStringValue(m)
	}

	return hstructpb.NewMapStringStringValue(hmaps.Concat(append([]map[string]string{m}, ms...)...))
}

func (p *Plugin) PluginInit(ctx context.Context, init pluginsdk.InitPayload) error {
	p.resultClient = init.Engine
	p.root = init.Root

	return nil
}

const Name = "go"

type Options struct {
	GoTool string `mapstructure:"gotool"`
}

func New(options Options) *Plugin {
	p := &Plugin{
		packageCache: cache.New(cache.AsLRU[packageCacheKey, *GetGoPackageCache](lru.WithCapacity(10000))),
		goTool:       options.GoTool,
	}

	return p
}

func (p *Plugin) Config(ctx context.Context, c *pluginv1.ProviderConfigRequest) (*pluginv1.ProviderConfigResponse, error) {
	return pluginv1.ProviderConfigResponse_builder{
		Name: htypes.Ptr(Name),
	}.Build(), nil
}

func (p *Plugin) Probe(ctx context.Context, c *pluginv1.ProbeRequest) (*pluginv1.ProbeResponse, error) {
	return &pluginv1.ProbeResponse{}, nil
}

func (p *Plugin) List(ctx context.Context, req *pluginv1.ListRequest) (pluginsdk.HandlerStreamReceive[*pluginv1.ListResponse], error) {
	return pluginsdk.NewChanHandlerStreamFunc(func(send func(*pluginv1.ListResponse) error) error {
		if _, ok := tref.CutPackagePrefix(req.GetPackage(), "@heph/go/std"); ok {
			// TODO: std

			return nil
		}

		if tref.HasPackagePrefix(req.GetPackage(), "@heph/go/thirdparty") {
			// TODO

			return nil
		}

		_, _, err := p.getGoModGoWork(ctx, req.GetPackage())
		if err != nil {
			if errors.Is(err, errNotInGoModule) {
				return nil
			}

			return err
		}

		// TODO: factors matrix
		for _, factors := range []Factors{{
			GoVersion: "1.25",
			GOOS:      runtime.GOOS,
			GOARCH:    runtime.GOARCH,
		}} {
			goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetPackage(), factors, req.GetRequestId())
			if err != nil {
				if errors.Is(err, errConstraintExcludeAllGoFiles) {
					continue
				}

				if errors.Is(err, errNoGoFiles) {
					return nil
				}

				return err
			}

			if len(goPkg.GoFiles) > 0 {
				err = send(pluginv1.ListResponse_builder{Ref: goPkg.GetBuildLibTargetRef(ModeNormal)}.Build())
				if err != nil {
					return err
				}
			}

			if goPkg.IsCommand() {
				err = send(pluginv1.ListResponse_builder{Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(goPkg.GetHephBuildPackage()),
					Name:    htypes.Ptr("build"),
					Args:    factors.Args(),
				}.Build()}.Build())
				if err != nil {
					return err
				}
			}

			if !goPkg.Is3rdParty && !goPkg.IsStd && (len(goPkg.TestGoFiles) > 0 || len(goPkg.XTestGoFiles) > 0) {
				err = send(pluginv1.ListResponse_builder{Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(goPkg.GetHephBuildPackage()),
					Name:    htypes.Ptr("test"),
					Args:    factors.Args(),
				}.Build()}.Build())
				if err != nil {
					return err
				}

				err = send(pluginv1.ListResponse_builder{Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(goPkg.GetHephBuildPackage()),
					Name:    htypes.Ptr("build_test"),
					Args:    factors.Args(),
				}.Build()}.Build())
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
	return ref.GetArgs()["mode"], nil
}

func (p *Plugin) Get(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.GetResponse, error) {
	if _, ok := tref.ParseFile(req.GetRef()); ok {
		return nil, pluginsdk.ErrNotFound
	}

	factors := FactorsFromArgs(req.GetRef().GetArgs())
	if factors.GoVersion == "" {
		factors.GoVersion = "1.25"
	}
	if factors.GOOS == "" {
		factors.GOOS = runtime.GOOS
	}
	if factors.GOARCH == "" {
		factors.GOARCH = runtime.GOARCH
	}

	if basePkg, modPath, version, modPkgPath, ok := ParseThirdpartyPackage(req.GetRef().GetPackage()); ok && basePkg == "" {
		if req.GetRef().GetName() == "download" {
			if modPkgPath != "" {
				return nil, errors.New("modpath is unsupported on download")
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

		return nil, pluginsdk.ErrNotFound
	}

	switch req.GetRef().GetName() {
	case "build":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.GetRequestId())
		if err != nil {
			return nil, err
		}

		gomod, _, err := p.getGoModGoWork(ctx, req.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		return p.packageBin(ctx, tref.DirPackage(gomod), goPkg, factors, req.GetRequestId())
	case "test":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.GetRequestId())
		if err != nil {
			return nil, err
		}

		return p.runTest(ctx, goPkg, factors, req.GetRef().GetArgs()["v"] == "1")
	case "embedcfg":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.GetRequestId())
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
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.GetRequestId())
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

		return p.packageLib(ctx, tref.DirPackage(gomod), goPkg, factors, mode, req.GetRequestId())
	case "build_test":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.GetRequestId())
		if err != nil {
			return nil, err
		}

		gomod, _, err := p.getGoModGoWork(ctx, req.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		return p.packageBinTest(ctx, tref.DirPackage(gomod), goPkg, factors, req.GetRequestId())
	case "testmain":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.GetRequestId())
		if err != nil {
			return nil, err
		}

		return p.generateTestMain(ctx, goPkg, factors)
	case "build_testmain_lib":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.GetRequestId())
		if err != nil {
			return nil, err
		}

		gomod, _, err := p.getGoModGoWork(ctx, req.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		return p.testMainLib(ctx, tref.DirPackage(gomod), goPkg, factors, req.GetRequestId())
	case "build_lib#asm":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.GetRequestId())
		if err != nil {
			return nil, err
		}

		mode, err := getMode(req.GetRef())
		if err != nil {
			return nil, fmt.Errorf("parse mode: %w", err)
		}

		return p.packageLibAsm(ctx, goPkg, factors, req.GetRef().GetArgs()["file"], mode)
	case "build_lib#abi":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.GetRequestId())
		if err != nil {
			return nil, err
		}

		mode, err := getMode(req.GetRef())
		if err != nil {
			return nil, fmt.Errorf("parse mode: %w", err)
		}

		return p.packageLibAbi(ctx, goPkg, factors, mode)
	case "build_lib#incomplete":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.GetRequestId())
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

		return p.packageLibIncomplete(ctx, tref.DirPackage(gomod), goPkg, factors, mode, req.GetRequestId())
	}

	return nil, pluginsdk.ErrNotFound
}

var errConstraintExcludeAllGoFiles = errors.New("build constraints exclude all Go files")
var errNoGoFiles = errors.New("no Go files in package")

func (p *Plugin) goListPkg(ctx context.Context, pkg string, f Factors, imp, requestId string) ([]*pluginv1.Artifact, *pluginv1.TargetRef, error) {
	gomod, gowork, err := p.getGoModGoWork(ctx, pkg)
	if err != nil {
		return nil, nil, err
	}

	var files []string
	if gomod != "" {
		files = append(files, tref.FormatFile(tref.DirPackage(gomod), tref.BasePackage(gomod)))
	}
	if gowork != "" {
		files = append(files, tref.FormatFile(tref.DirPackage(gowork), tref.BasePackage(gowork)))
	}

	args := f.Args()
	if imp == "." {
		entries, err := os.ReadDir(filepath.Join(p.root, pkg))
		if err != nil {
			return nil, nil, err
		}

		var hasGoFile bool
		for _, e := range entries {
			if e.IsDir() {
				continue
			}
			files = append(files, tref.FormatFile(pkg, e.Name()))

			if !hasGoFile && strings.HasSuffix(e.Name(), ".go") {
				hasGoFile = true
			}
		}

		if !hasGoFile {
			return nil, nil, errNoGoFiles
		}

		files = append(files, tref.FormatQuery(tref.QueryOptions{
			Label:        "go_src",
			SkipProvider: Name,
			TreeOutputTo: pkg,
		}))
	} else {
		args = hmaps.Concat(args, map[string]string{
			"import": imp,
		})
	}

	listRef := pluginv1.TargetRef_builder{
		Package: htypes.Ptr(pkg),
		Name:    htypes.Ptr("_golist"),
		Args:    args,
	}.Build()

	res, err := p.resultClient.ResultClient.Get(ctx, corev1.ResultRequest_builder{
		RequestId: htypes.Ptr(requestId),
		Spec: pluginv1.TargetSpec_builder{
			Ref:    listRef,
			Driver: htypes.Ptr("sh"),
			Config: map[string]*structpb.Value{
				"env":              p.getEnvStructpb(f),
				"runtime_pass_env": p.getRuntimePassEnvStructpb(),
				"run":              structpb.NewStringValue(fmt.Sprintf("go list -mod=readonly -json -tags %q %v > $OUT", f.Tags, imp)),
				"out":              structpb.NewStringValue("golist.json"),
				"in_tree":          structpb.NewBoolValue(true),
				"cache":            structpb.NewStringValue("local"),
				"hash_deps":        hstructpb.NewStringsValue(files),
				"tools":            p.getGoToolStructpb(),
			},
		}.Build(),
	}.Build())
	if err != nil {
		if strings.Contains(err.Error(), "build constraints exclude all Go files") {
			return nil, nil, errConstraintExcludeAllGoFiles
		}

		return nil, nil, fmt.Errorf("golist: %v (in %v): %v: %w", imp, pkg, tref.Format(listRef), err)
	}

	return res.GetArtifacts(), res.GetDef().GetRef(), nil
}
