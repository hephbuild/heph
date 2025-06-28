package plugingo

import (
	"connectrpc.com/connect"
	"context"
	"errors"
	"fmt"
	"github.com/goccy/go-json"
	"github.com/hephbuild/heph/herrgroup"
	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/internal/hslices"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	sync_map "github.com/zolstein/sync-map"
	"google.golang.org/protobuf/types/known/structpb"
	"io"
	"os"
	"path"
	"path/filepath"
	"slices"
	"strings"
	"sync"
)

func (p *Plugin) goListPkgResult(ctx context.Context, basePkg, runPkg, imp string, factors Factors, requestId string) (Package, error) {
	artifacts, _, err := p.goListPkg(ctx, runPkg, factors, imp, requestId)
	if err != nil {
		return Package{}, fmt.Errorf("go list: %w", err)
	}

	outputArtifacts := hartifact.FindOutputs(artifacts, "")

	if len(outputArtifacts) == 0 {
		return Package{}, connect.NewError(connect.CodeInternal, errors.New("golist: no output found"))
	}

	outputArtifact := outputArtifacts[0]

	f, err := hartifact.FileReader(ctx, outputArtifact)
	if err != nil {
		return Package{}, err
	}
	defer f.Close()

	var goPkg Package
	err = json.NewDecoder(f).Decode(&goPkg)
	if err != nil {
		return Package{}, err
	}

	relPkg, err := tref.DirToPackage(goPkg.Dir, p.root)
	if err != nil {
		goPkg.Is3rdParty = true

		if goPkg.Module == nil {
			return Package{}, fmt.Errorf("%v: not in a module", imp)
		}

		modPath := strings.ReplaceAll(goPkg.ImportPath, goPkg.Module.Path, "")
		modPath = strings.TrimPrefix(modPath, "/")

		goPkg.HephPackage = ThirdpartyBuildPackage(basePkg, goPkg.Module.Path, goPkg.Module.Version, modPath)
		goPkg.HephBuildPackage = ThirdpartyBuildPackage(basePkg, goPkg.Module.Path, goPkg.Module.Version, modPath)
	} else {
		goPkg.HephPackage = relPkg
	}
	goPkg.Factors = factors

	return goPkg, nil
}

type GetGoPackageCache struct {
	stdListRes func() (map[string]Package, error)
	modulesRes func() ([]Module, error)
	basePkg    string
}

func (p *Plugin) newGetGoPackageCache(ctx context.Context, basePkg string, factors Factors, requestId string) *GetGoPackageCache {
	c := &GetGoPackageCache{
		basePkg: basePkg,
		stdListRes: func() (map[string]Package, error) {
			res, err, _ := p.stdCache.Do(stdCacheKey{
				RequestId: requestId,
				Factors:   factors,
			}, func() (map[string]Package, error) {
				stdList, err := p.resultStdList(ctx, factors, requestId)
				if err != nil {
					return nil, err
				}

				stdListMap := make(map[string]Package, len(stdList))
				for _, stdPkg := range stdList {
					stdListMap[stdPkg.ImportPath] = stdPkg
				}
				return stdListMap, nil
			})

			return res, err
		},
		modulesRes: func() ([]Module, error) {
			res, err, _ := p.moduleCache.Do(moduleCacheKey{
				RequestId: requestId,
				Factors:   factors,
				BasePkg:   basePkg,
			}, func() ([]Module, error) {
				return p.goModules(ctx, basePkg, requestId)
			})

			return res, err
		},
	}

	c, _ = p.packageCache.GetOrSet(packageCacheKey{
		RequestId: requestId,
		Factors:   factors,
		BasePkg:   basePkg,
	}, c)

	return c
}

func ParseThirdpartyPackage(pkg string) (string, string, string, string, bool) {
	if basePkg, rest, ok := tref.CutPackage(pkg, ThirdpartyPrefix); ok {
		modPath, rest, _ := strings.Cut(rest, "@")
		version, modPkgPath, _ := strings.Cut(rest, "/")

		return basePkg, modPath, version, modPkgPath, true
	}

	return "", "", "", "", false
}

func (p *Plugin) getGoPackageFromHephPackage(ctx context.Context, pkg string, factors Factors, requestId string) (Package, error) {
	if basePkg, modPath, version, modPkgPath, ok := ParseThirdpartyPackage(pkg); ok {
		goPkg, err := p.goListPkgResult(ctx, basePkg, basePkg, path.Join(modPath, modPkgPath), factors, requestId)
		if err != nil {
			return Package{}, fmt.Errorf("thirdparty: %w", err)
		}

		if goPkg.Module.Version != version {
			return Package{}, fmt.Errorf("version mismatch %v %v", goPkg.Module.Version, version)
		}

		return goPkg, nil
	}

	gomod, _, err := p.getGoModGoWork(ctx, pkg)
	if err != nil {
		return Package{}, err
	}

	basePkg := tref.DirPackage(gomod)

	c := p.newGetGoPackageCache(ctx, basePkg, factors, requestId)

	stdList, err := c.stdListRes()
	if err != nil {
		return Package{}, err
	}

	for _, p := range stdList {
		if p.HephPackage == pkg {
			return p, nil
		}
	}

	goPkg, err := p.goListPkgResult(ctx, basePkg, pkg, ".", factors, requestId)
	if err != nil {
		return Package{}, fmt.Errorf("in tree: %w", err)
	}

	return goPkg, nil
}

func (p *Plugin) getGoTestmainPackageFromImportPath(ctx context.Context, imp string, factors Factors, c *GetGoPackageCache, requestId string) (LibPackage, error) {
	goPkg, err := p.getGoPackageFromImportPath(ctx, imp, factors, c, requestId)
	if err != nil {
		return LibPackage{}, err
	}

	//hasTest := len(goPkg.TestGoFiles) > 0
	//hasXTest := len(goPkg.XTestGoFiles) > 0

	goPkg.ImportPath = goPkg.ImportPath + "_testmain"
	goPkg.Name = MainPackage
	goPkg.EmbedPatterns = nil
	goPkg.EmbedPatternPos = nil
	goPkg.TestEmbedPatterns = nil
	goPkg.TestEmbedPatternPos = nil
	goPkg.XTestEmbedPatterns = nil
	goPkg.XTestEmbedPatternPos = nil
	goPkg.GoFiles = nil
	goPkg.CgoFiles = nil
	goPkg.IgnoredGoFiles = nil
	goPkg.InvalidGoFiles = nil
	goPkg.IgnoredOtherFiles = nil
	goPkg.CFiles = nil
	goPkg.CXXFiles = nil
	goPkg.MFiles = nil
	goPkg.HFiles = nil
	goPkg.FFiles = nil
	goPkg.SFiles = nil
	goPkg.SwigFiles = nil
	goPkg.SwigCXXFiles = nil
	goPkg.SysoFiles = nil
	goPkg.TestGoFiles = nil
	goPkg.XTestGoFiles = nil

	goPkg.LibTargetRef = &pluginv1.TargetRef{
		Package: goPkg.GetHephBuildPackage(),
		Name:    "build_testmain_lib",
		Args:    factors.Args(),
	}
	goPkg.Imports = slices.Clone(testmainImports)

	//if hasTest {
	//	goPkg.Imports = append(goPkg.Imports, xxx)
	//}
	//if hasXTest {
	//	goPkg.Imports = append(goPkg.Imports, xxx)
	//}

	return LibPackage{
		Mode:       ModeNormal,
		Imports:    goPkg.Imports,
		GoPkg:      goPkg,
		ImportPath: MainPackage,
		Name:       MainPackage,
		LibTargetRef: &pluginv1.TargetRef{
			Package: goPkg.GetHephBuildPackage(),
			Name:    "build_testmain_lib",
			Args:    factors.Args(),
		},
	}, nil
}

func (p *Plugin) getGoPackageFromImportPath(ctx context.Context, imp string, factors Factors, c *GetGoPackageCache, requestId string) (Package, error) {
	stdList, err := c.stdListRes()
	if err != nil {
		return Package{}, err
	}

	if stdPkg, isStd := stdList[imp]; isStd {
		return stdPkg, nil
	}

	modules, err := c.modulesRes()
	if err != nil {
		return Package{}, fmt.Errorf("get modules list: %w", err)
	}

	var hephPkg string
	for _, module := range modules {
		if rest, ok := strings.CutPrefix(imp, module.Path); ok {
			hephPkg = tref.JoinPackage(module.HephPackage, strings.TrimLeft(rest, "/"))
			break
		}
	}

	if hephPkg == "" {
		// Attempt to download 3rdparty package
		goPkg, err := p.goListPkgResult(ctx, c.basePkg, c.basePkg, imp, factors, requestId)
		if err != nil {
			return Package{}, err
		}

		return goPkg, nil
	}

	return p.getGoPackageFromHephPackage(ctx, hephPkg, factors, requestId)
}

func (p *Plugin) goListTestDepsPkgResult(ctx context.Context, pkg string, factors Factors, c *GetGoPackageCache, extraImports []string, requestId string) ([]LibPackage, error) {
	goPkg, err := p.getGoPackageFromHephPackage(ctx, pkg, factors, requestId)
	if err != nil {
		return nil, fmt.Errorf("get pkg: %w", err)
	}

	var imports []string
	imports = append(imports, goPkg.TestImports...)
	imports = append(imports, goPkg.XTestImports...)
	imports = append(imports, extraImports...)

	goPkgs := make([]LibPackage, 0)
	var goPkgsm sync.Mutex
	var g herrgroup.Group

	g.Go(func() error {
		if len(goPkg.TestGoFiles) > 0 {
			g.Go(func() error {
				libGoPkg, err := p.libGoPkg(ctx, goPkg, ModeTest)
				if err != nil {
					return err
				}

				goPkgsm.Lock()
				goPkgs = append(goPkgs, libGoPkg)
				goPkgsm.Unlock()

				res, err := p.goImportsToDeps(ctx, libGoPkg.Imports, factors, c, requestId, nil)
				if err != nil {
					return fmt.Errorf("get deps: %w", err)
				}

				goPkgsm.Lock()
				goPkgs = append(goPkgs, res...)
				goPkgsm.Unlock()

				return nil
			})
		}

		if len(goPkg.XTestGoFiles) > 0 {
			g.Go(func() error {
				libGoPkg, err := p.libGoPkg(ctx, goPkg, ModeXTest)
				if err != nil {
					return err
				}

				goPkgsm.Lock()
				goPkgs = append(goPkgs, libGoPkg)
				goPkgsm.Unlock()

				return nil
			})
		}

		return nil
	})

	g.Go(func() error {
		res, err := p.goImportsToDeps(ctx, imports, factors, c, requestId, nil)
		if err != nil {
			return fmt.Errorf("get deps: %w", err)
		}

		goPkgsm.Lock()
		goPkgs = append(goPkgs, res...)
		goPkgsm.Unlock()

		return nil
	})

	err = g.Wait()
	if err != nil {
		return nil, err
	}

	slices.SortFunc(goPkgs, func(a, b LibPackage) int {
		return strings.Compare(a.ImportPath, b.ImportPath)
	})

	goPkgs = hslices.UniqBy(goPkgs, func(item LibPackage) string {
		return item.ImportPath
	})

	return goPkgs, nil
}

func (p *Plugin) goImportsToGoPkgs(ctx context.Context, imports []string, factors Factors, c *GetGoPackageCache, requestId string) ([]Package, error) {
	goPkgs := make([]Package, len(imports))
	var g herrgroup.Group

	for i, imp := range imports {
		g.Go(func() error {
			impGoPkg, err := p.getGoPackageFromImportPath(ctx, imp, factors, c, requestId)
			if err != nil {
				return fmt.Errorf("get pkg: %w", err)
			}

			goPkgs[i] = impGoPkg

			return nil
		})
	}

	err := g.Wait()
	if err != nil {
		return nil, err
	}

	return goPkgs, nil
}

func (p *Plugin) goImportsToDeps(ctx context.Context, imports []string, factors Factors, c *GetGoPackageCache, requestId string, seen *sync_map.Map[string, struct{}]) ([]LibPackage, error) {
	goPkgs := make([]LibPackage, 0)
	var goPkgsm sync.Mutex
	var g herrgroup.Group

	if seen == nil {
		seen = &sync_map.Map[string, struct{}]{}
	}

	for _, imp := range imports {
		if _, loaded := seen.LoadOrStore(imp, struct{}{}); loaded {
			continue
		}

		g.Go(func() error {
			impGoPkg, err := p.getGoPackageFromImportPath(ctx, imp, factors, c, requestId)
			if err != nil {
				return fmt.Errorf("get pkg: %v: %w", imp, err)
			}

			g.Go(func() error {
				depPkgs, err := p.goImportsToDeps(ctx, impGoPkg.Imports, factors, c, requestId, seen)
				if err != nil {
					return fmt.Errorf("get deps: %v: %w", impGoPkg, err)
				}

				goPkgsm.Lock()
				goPkgs = append(goPkgs, depPkgs...)
				goPkgsm.Unlock()

				return nil
			})

			libImpGoPkg, err := p.libGoPkg(ctx, impGoPkg, ModeNormal)
			if err != nil {
				return err
			}

			goPkgsm.Lock()
			goPkgs = append(goPkgs, libImpGoPkg)
			goPkgsm.Unlock()

			return nil
		})
	}

	err := g.Wait()
	if err != nil {
		return nil, err
	}

	slices.SortFunc(goPkgs, func(a, b LibPackage) int {
		return strings.Compare(a.ImportPath, b.ImportPath)
	})

	goPkgs = hslices.UniqBy(goPkgs, func(item LibPackage) string {
		return item.ImportPath
	})

	return goPkgs, nil
}

var errNotInGoModule = errors.New("not in go module")

func (p *Plugin) getGoModGoWork(ctx context.Context, pkg string) (string, string, error) {
	res, err, _ := p.goModGoWorkCache.Do(pkg, sync.OnceValues(func() (goModGoWorkCache, error) {
		gomod, gowork, err := p.getGoModGoWorkInner(ctx, pkg)
		if err != nil {
			return goModGoWorkCache{}, err
		}

		return goModGoWorkCache{
			gomod:  gomod,
			gowork: gowork,
		}, nil
	}))
	if err != nil {
		return "", "", err
	}

	return res.gomod, res.gowork, nil
}

func (p *Plugin) getGoModGoWorkInner(ctx context.Context, pkg string) (string, string, error) {
	pkgParts := tref.SplitPackage(pkg)

	var gomod, gowork string

	for {
		pkg := tref.JoinPackage(pkgParts...)
		pkgDirParts := append([]string{p.root}, pkgParts...)
		pkgDir := filepath.Join(pkgDirParts...)

		if gomod == "" {
			_, err := os.Stat(filepath.Join(pkgDir, "go.mod"))
			if err == nil {
				gomod = tref.JoinPackage(pkg, "go.mod")
			}
		}

		if gowork == "" {
			_, err := os.Stat(filepath.Join(pkgDir, "go.work"))
			if err == nil {
				gowork = tref.JoinPackage(pkg, "go.work")
			}
		}

		if gomod != "" && gowork != "" {
			break
		}

		if len(pkgParts) == 0 {
			break
		}

		pkgParts = pkgParts[:len(pkgParts)-1]
	}

	if gomod == "" {
		return "", "", fmt.Errorf("%v: %w", pkg, errNotInGoModule)
	}

	return gomod, gowork, nil
}

func (p *Plugin) goModules(ctx context.Context, pkg, requestId string) ([]Module, error) {
	gomod, gowork, err := p.getGoModGoWork(ctx, pkg)
	if err != nil {
		return nil, err
	}

	files := []string{
		tref.FormatFile(tref.DirPackage(gomod), tref.BasePackage(gomod)),
	}
	if gowork != "" {
		files = append(files, tref.FormatFile(tref.DirPackage(gomod), tref.BasePackage(gowork)))
	}

	res, err := p.resultClient.ResultClient.Get(ctx, &corev1.ResultRequest{
		RequestId: requestId,
		Of: &corev1.ResultRequest_Spec{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: tref.DirPackage(gomod),
					Name:    "_gomod",
				},
				Driver: "sh",
				Config: map[string]*structpb.Value{
					"runtime_pass_env": hstructpb.NewStringsValue([]string{"HOME"}),
					"run":              structpb.NewStringValue("go list -m -json > $OUT"),
					"out":              structpb.NewStringValue("golist_mod.json"),
					"in_tree":          structpb.NewBoolValue(true),
					"cache":            structpb.NewStringValue("local"),
					"hash_deps":        hstructpb.NewStringsValue(files),
					// "tools": hstructpb.NewStringsValue([]string{fmt.Sprintf("//go_toolchain/%v:go", f.GoVersion)}),
				},
			},
		},
	})
	if err != nil {
		return nil, fmt.Errorf("gomod: %w", err)
	}

	outputArtifacts := hartifact.FindOutputs(res.GetArtifacts(), "")

	if len(outputArtifacts) == 0 {
		return nil, connect.NewError(connect.CodeInternal, errors.New("gomodules: no output found"))
	}

	outputArtifact := outputArtifacts[0]

	f, err := hartifact.FileReader(ctx, outputArtifact)
	if err != nil {
		return nil, err
	}
	defer f.Close()

	var modules []Module

	dec := json.NewDecoder(f)
	for {
		var mod Module
		err := dec.Decode(&mod)
		if err == io.EOF {
			break
		}
		if err != nil {
			return nil, err
		}

		mod.HephPackage, err = tref.DirToPackage(mod.Dir, p.root)
		if err != nil {
			return nil, err
		}

		modules = append(modules, mod)
	}

	return modules, nil
}
