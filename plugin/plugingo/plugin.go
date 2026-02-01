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

type Plugin struct {
	goTool           string
	resultClient     pluginsdk.Engine
	root             string
	resultStdListMem hsingleflight.GroupMem[Factors, []Package]

	packageCache     *cache.Cache[packageCacheKey, *GetGoPackageCache]
	moduleCache      hsingleflight.GroupMem[moduleCacheKey, []Module]
	stdCache         hsingleflight.GroupMem[stdCacheKey, map[string]Package]
	goModGoWorkCache hsingleflight.GroupMemContext[string, goModRoot]
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

// getCodegenRoot extracts the codegen root package from provider states
func getCodegenRoot(states []*pluginv1.ProviderState, providerName string) string {
	for _, state := range states {
		if state.GetProvider() == providerName {
			if isCodegenRoot := state.GetState()["go_codegen_root"]; isCodegenRoot != nil && isCodegenRoot.GetBoolValue() {
				return state.GetPackage()
			}
		}
	}
	return ""
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

		_, err := p.getGoModGoWork(ctx, req.GetPackage())
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
				err = send(pluginv1.ListResponse_builder{Ref: tref.New(goPkg.GetHephBuildPackage(), "build", factors.Args())}.Build())
				if err != nil {
					return err
				}
			}

			if !goPkg.Is3rdParty && !goPkg.IsStd && (len(goPkg.TestGoFiles) > 0 || len(goPkg.XTestGoFiles) > 0) {

				err = send(pluginv1.ListResponse_builder{Ref: tref.New(goPkg.GetHephBuildPackage(), "test", factors.Args())}.Build())
				if err != nil {
					return err
				}

				err = send(pluginv1.ListResponse_builder{Ref: tref.New(goPkg.GetHephBuildPackage(), "build_test", factors.Args())}.Build())
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
	if req.GetRef().GetPackage() == tref.FSPackage {
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

	if req.GetRef().GetName() == "_golist" {
		imp := req.GetRef().GetArgs()["imp"]

		return p.goList(ctx, req.GetRef().GetPackage(), factors, imp, req.GetStates())
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

		gomod, err := p.getGoModGoWork(ctx, req.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		return p.packageBin(ctx, gomod.basePkg, goPkg, factors, req.GetRequestId())
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

		gomod, err := p.getGoModGoWork(ctx, req.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		mode, err := getMode(req.GetRef())
		if err != nil {
			return nil, fmt.Errorf("parse mode: %w", err)
		}

		return p.embedCfg(ctx, gomod.basePkg, req.GetRef().GetPackage(), goPkg, factors, mode)
	case "build_lib":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.GetRequestId())
		if err != nil {
			return nil, err
		}

		gomod, err := p.getGoModGoWork(ctx, req.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		mode, err := getMode(req.GetRef())
		if err != nil {
			return nil, fmt.Errorf("parse mode: %w", err)
		}

		return p.packageLib(ctx, gomod.basePkg, goPkg, factors, mode, req.GetRequestId())
	case "build_test":
		goPkg, err := p.getGoPackageFromHephPackage(ctx, req.GetRef().GetPackage(), factors, req.GetRequestId())
		if err != nil {
			return nil, err
		}

		gomod, err := p.getGoModGoWork(ctx, req.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		return p.packageBinTest(ctx, gomod.basePkg, goPkg, factors, req.GetRequestId())
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

		gomod, err := p.getGoModGoWork(ctx, req.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		return p.testMainLib(ctx, gomod.basePkg, goPkg, factors, req.GetRequestId())
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

		gomod, err := p.getGoModGoWork(ctx, req.GetRef().GetPackage())
		if err != nil {
			return nil, err
		}

		mode, err := getMode(req.GetRef())
		if err != nil {
			return nil, fmt.Errorf("parse mode: %w", err)
		}

		return p.packageLibIncomplete(ctx, gomod.basePkg, goPkg, factors, mode, req.GetRequestId())
	}

	return nil, pluginsdk.ErrNotFound
}

var errNoGoFiles = errors.New("no Go files in package")

func (p *Plugin) goList(ctx context.Context, pkg string, f Factors, imp string, states []*pluginv1.ProviderState) (*pluginv1.GetResponse, error) {
	gomod, err := p.getGoModGoWork(ctx, pkg)
	if err != nil {
		return nil, err
	}

	files := []string{}
	files = append(files, gomod.goModFileDeps...)
	files = append(files, gomod.goWorkFileDeps...)

	args := f.Args()
	args = hmaps.Concat(args, map[string]string{
		"imp": imp,
	})

	var inTree bool

	if imp == "." {
		// Determine package prefix from probe states
		packagePrefix := pkg // default to current package
		if codegenRoot := getCodegenRoot(states, Name); codegenRoot != "" {
			packagePrefix = codegenRoot
		}

		entries, err := os.ReadDir(filepath.Join(p.root, pkg))
		if err != nil {
			return nil, err
		}

		var hasGoFile bool
		for _, e := range entries {
			if e.IsDir() {
				continue
			}

			if strings.HasSuffix(e.Name(), ".go") {
				hasGoFile = true
				break
			}
		}

		if !hasGoFile {
			return nil, errNoGoFiles
		}

		files = append(files, tref.FormatGlob(pkg, "*.go", nil))

		files = append(files, tref.FormatQuery(tref.QueryOptions{
			Label:         "go_src",
			SkipProvider:  Name,
			TreeOutputTo:  pkg,
			PackagePrefix: packagePrefix, // Use determined prefix instead of pkg
		}))

		inTree = true
		// directories from heph v0 linked to tree will cause the exec to be in the .heph cache dir, causing wrong go mod
		if _, err := os.Readlink(filepath.Join(p.root, tref.ToOSPath(pkg))); err == nil {
			inTree = false
		}
	}

	var rootVar string
	if inTree {
		rootVar = "$TREE_ROOT"
	} else {
		rootVar = "$WORKSPACE_ROOT"
	}

	return pluginv1.GetResponse_builder{
		Spec: pluginv1.TargetSpec_builder{
			Ref:    tref.New(pkg, "_golist", args),
			Driver: htypes.Ptr("sh"),
			Config: map[string]*structpb.Value{
				"env":              p.getEnvStructpb(f),
				"runtime_pass_env": p.getRuntimePassEnvStructpb(),
				"run": hstructpb.NewStringsValue([]string{
					fmt.Sprintf("go list -mod=readonly -json -tags %q %v > $OUT_JSON", f.Tags, imp),
					"echo " + rootVar + " > $OUT_ROOT",
				}),
				"out": hstructpb.NewMapStringStringValue(map[string]string{
					"json": "golist.json",
					"root": "golist_root",
				}),
				"in_tree":   structpb.NewBoolValue(inTree),
				"cache":     structpb.NewStringValue("local"),
				"hash_deps": hstructpb.NewStringsValue(files),
				"tools":     p.getGoToolStructpb(),
			},
		}.Build(),
	}.Build(), nil
}
