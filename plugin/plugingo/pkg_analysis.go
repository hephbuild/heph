package plugingo

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"path"
	"path/filepath"
	"slices"
	"strings"
	"sync"

	"github.com/hephbuild/heph/internal/hdebug"
	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/hephbuild/heph/internal/htypes"

	"github.com/hephbuild/heph/internal/herrgroup"
	"github.com/hephbuild/heph/lib/tref"

	"connectrpc.com/connect"
	"github.com/goccy/go-json"
	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/internal/hslices"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	sync_map "github.com/zolstein/sync-map"
	"google.golang.org/protobuf/types/known/structpb"
)

var errConstraintExcludeAllGoFiles = errors.New("build constraints exclude all Go files")

func (p *Plugin) goListPkg(ctx context.Context, pkg string, factors Factors, imp, requestId string) ([]*pluginv1.Artifact, *pluginv1.TargetRef, error) {
	res, err := p.resultClient.ResultClient.Get(ctx, corev1.ResultRequest_builder{
		RequestId: htypes.Ptr(requestId),
		Ref: tref.New(pkg, "_golist", hmaps.Concat(factors.Args(), map[string]string{
			"imp": imp,
		})),
	}.Build())
	if err != nil {
		if strings.Contains(err.Error(), "build constraints exclude all Go files") {
			return nil, nil, fmt.Errorf("%w: %w", errConstraintExcludeAllGoFiles, err)
		}

		return nil, nil, fmt.Errorf("golist: %v (in %v): %w", imp, pkg, err)
	}

	return res.GetArtifacts(), res.GetDef().GetRef(), nil
}

func (p *Plugin) goListPkgResult(ctx context.Context, basePkg, runPkg, imp string, factors Factors, requestId string) (Package, error) {
	artifacts, _, err := p.goListPkg(ctx, runPkg, factors, imp, requestId)
	if err != nil {
		return Package{}, fmt.Errorf("go list: %w", err)
	}

	jsonArtifacts := hartifact.FindOutputs(artifacts, "json")
	rootArtifacts := hartifact.FindOutputs(artifacts, "root")

	if len(jsonArtifacts) == 0 || len(rootArtifacts) == 0 {
		return Package{}, connect.NewError(connect.CodeInternal, errors.New("golist: no json found"))
	}

	jsonArtifact := jsonArtifacts[0]
	rootArtifact := rootArtifacts[0]

	rootb, err := hartifact.FileReadAll(ctx, rootArtifact)
	if err != nil {
		return Package{}, err
	}

	root := strings.TrimSpace(string(rootb))

	f, err := hartifact.FileReader(ctx, jsonArtifact)
	if err != nil {
		return Package{}, err
	}
	defer f.Close()

	var goPkg Package
	err = json.NewDecoder(f).Decode(&goPkg)
	if err != nil {
		return Package{}, err
	}

	relPkg, err := tref.DirToPackage(goPkg.Dir, root)
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
	key := packageCacheKey{
		RequestId: requestId,
		Factors:   factors,
		BasePkg:   basePkg,
	}

	c, ok := p.packageCache.Get(key)
	if ok {
		return c
	}

	c, _ = p.packageCache.GetOrSet(key, &GetGoPackageCache{
		basePkg: basePkg,
		stdListRes: func() (map[string]Package, error) {
			res, err, _ := p.stdCache.Do(stdCacheKey{
				//RequestId: requestId, TODO: bring back with a "master request id"
				Factors: factors,
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
				//RequestId: requestId, TODO: bring back with a "master request id"
				Factors: factors,
				BasePkg: basePkg,
			}, func() ([]Module, error) {
				return p.goModules(ctx, basePkg, requestId)
			})

			return res, err
		},
	})

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

	gomod, err := p.getGoModGoWork(ctx, pkg)
	if err != nil {
		return Package{}, err
	}

	c := p.newGetGoPackageCache(ctx, gomod.basePkg, factors, requestId)

	stdList, err := c.stdListRes()
	if err != nil {
		return Package{}, err
	}

	for _, p := range stdList {
		if p.HephPackage == pkg {
			return p, nil
		}
	}

	goPkg, err := p.goListPkgResult(ctx, gomod.basePkg, pkg, ".", factors, requestId)
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

	// hasTest := len(goPkg.TestGoFiles) > 0
	// hasXTest := len(goPkg.XTestGoFiles) > 0

	goPkg.ImportPath += "_testmain"
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

	goPkg.LibTargetRef = tref.New(goPkg.GetHephBuildPackage(), "build_testmain_lib", factors.Args())
	goPkg.Imports = slices.Clone(testmainImports)

	// if hasTest {
	//	goPkg.Imports = append(goPkg.Imports, xxx)
	//}
	// if hasXTest {
	//	goPkg.Imports = append(goPkg.Imports, xxx)
	//}

	return LibPackage{
		Mode:         ModeNormal,
		Imports:      goPkg.Imports,
		GoPkg:        goPkg,
		ImportPath:   MainPackage,
		Name:         MainPackage,
		LibTargetRef: tref.New(goPkg.GetHephBuildPackage(), "build_testmain_lib", factors.Args()),
	}, nil
}

func (p *Plugin) getGoPackageFromImportPath(ctx context.Context, imp string, factors Factors, c *GetGoPackageCache, requestId string) (Package, error) {
	ctx, cleanLabels := hdebug.SetLabels(ctx, func() []string {
		return []string{"where", "getGoPackageFromImportPath " + imp}
	})
	defer cleanLabels()

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

func (p *Plugin) goListTestDepsPkgResult(
	ctx context.Context,
	pkg string,
	factors Factors,
	c *GetGoPackageCache,
	extraImports []string,
	requestId string,
) ([]LibPackage, error) {
	goPkg, err := p.getGoPackageFromHephPackage(ctx, pkg, factors, requestId)
	if err != nil {
		return nil, fmt.Errorf("get pkg: %w", err)
	}

	var imports []string
	imports = append(imports, goPkg.TestImports...)
	imports = append(imports, goPkg.XTestImports...)
	imports = append(imports, extraImports...)

	if len(goPkg.TestGoFiles) > 0 || len(goPkg.XTestGoFiles) > 0 {
		imports = slices.DeleteFunc(imports, func(imp string) bool {
			return imp == goPkg.ImportPath
		})
	}

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

				// Add the test mode version of the package under test
				// Only add it if the package has GoFiles or TestGoFiles (not xtest-only)
				if len(goPkg.GoFiles) > 0 || len(goPkg.TestGoFiles) > 0 {
					testGoPkg, err := p.libGoPkg(ctx, goPkg, ModeTest)
					if err != nil {
						return err
					}
					goPkgsm.Lock()
					goPkgs = append(goPkgs, testGoPkg)
					goPkgsm.Unlock()

					// Collect dependencies of the test mode package
					testDeps, err := p.goImportsToDeps(ctx, testGoPkg.Imports, factors, c, requestId, nil)
					if err != nil {
						return fmt.Errorf("get test deps: %w", err)
					}

					goPkgsm.Lock()
					goPkgs = append(goPkgs, testDeps...)
					goPkgsm.Unlock()
				}

				// Filter out the package under test from xtest imports before processing
				filteredImports := slices.DeleteFunc(slices.Clone(libGoPkg.Imports), func(imp string) bool {
					return imp == goPkg.ImportPath
				})

				res, err := p.goImportsToDeps(ctx, filteredImports, factors, c, requestId, nil)
				if err != nil {
					return fmt.Errorf("get deps: %w", err)
				}

				goPkgsm.Lock()
				goPkgs = append(goPkgs, res...)
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

	// Deduplicate by ImportPath, preferring Mode Test over ModeNormal
	seen := make(map[string]int)
	deduped := make([]LibPackage, 0, len(goPkgs))
	for _, pkg := range goPkgs {
		if idx, exists := seen[pkg.ImportPath]; exists {
			// If we already have this import path, keep ModeTest if either is ModeTest
			if pkg.Mode == ModeTest || deduped[idx].Mode == ModeTest {
				deduped[idx] = LibPackage{
					Mode:          ModeTest,
					Imports:       pkg.Imports,
					EmbedPatterns: pkg.EmbedPatterns,
					GoFiles:       pkg.GoFiles,
					GoPkg:         pkg.GoPkg,
					ImportPath:    pkg.ImportPath,
					Name:          pkg.Name,
					LibTargetRef:  pkg.GoPkg.GetBuildLibTargetRef(ModeTest),
				}
			}
		} else {
			seen[pkg.ImportPath] = len(deduped)
			deduped = append(deduped, pkg)
		}
	}
	goPkgs = deduped

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

func (p *Plugin) goImportsToDeps(
	ctx context.Context,
	imports []string,
	factors Factors,
	c *GetGoPackageCache,
	requestId string,
	seen *sync_map.Map[string, struct{}],
) ([]LibPackage, error) {
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
					return fmt.Errorf("get deps: %w", err)
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

type goModRoot struct {
	basePkg        string
	goModFileDeps  []string
	goWorkFileDeps []string
}

func (mr goModRoot) hasMod() bool {
	return len(mr.goModFileDeps) > 0
}

func (mr goModRoot) hasWork() bool {
	return len(mr.goWorkFileDeps) > 0
}

func (p *Plugin) getGoModGoWork(ctx context.Context, pkg string) (goModRoot, error) {
	res, err, _ := p.goModGoWorkCache.Do(ctx, pkg, func(ctx context.Context) (goModRoot, error) {
		return p.getGoModGoWorkInner(ctx, pkg)
	})
	if err != nil {
		return goModRoot{}, err
	}

	return res, nil
}

func (p *Plugin) getGoModGoWorkInner(ctx context.Context, pkg string) (goModRoot, error) {
	mr := goModRoot{}

	gowork := false

	for pkg := range tref.ParentPackages(pkg) {
		pkgDir := filepath.Join(p.root, tref.ToOSPath(pkg))

		if !mr.hasMod() {
			_, err := os.Stat(filepath.Join(pkgDir, "go.mod"))
			if err == nil {
				mr.goModFileDeps = append(mr.goModFileDeps, tref.FormatFile(pkg, "go.mod"))
				mr.basePkg = pkg
			}

			_, err = os.Stat(filepath.Join(pkgDir, "go.sum"))
			if err == nil {
				mr.goModFileDeps = append(mr.goModFileDeps, tref.FormatFile(pkg, "go.sum"))
			}
		}

		if gowork && mr.hasMod() {
			_, err := os.Stat(filepath.Join(pkgDir, "go.work"))
			if err == nil {
				mr.goWorkFileDeps = append(mr.goWorkFileDeps, tref.FormatFile(pkg, "go.work"))
			}

			_, err = os.Stat(filepath.Join(pkgDir, "go.work.sum"))
			if err == nil {
				mr.goWorkFileDeps = append(mr.goWorkFileDeps, tref.FormatFile(pkg, "go.work.sum"))
			}
		}

		if mr.hasMod() && (!gowork || mr.hasWork()) {
			break
		}
	}

	if !mr.hasMod() {
		return goModRoot{}, fmt.Errorf("%v: %w", pkg, errNotInGoModule)
	}

	return mr, nil
}

func (p *Plugin) goModules(ctx context.Context, pkg, requestId string) ([]Module, error) {
	gomod, err := p.getGoModGoWork(ctx, pkg)
	if err != nil {
		return nil, err
	}

	files := []string{}
	files = append(files, gomod.goModFileDeps...)
	files = append(files, gomod.goWorkFileDeps...)

	res, err := p.resultClient.ResultClient.Get(ctx, corev1.ResultRequest_builder{
		RequestId: htypes.Ptr(requestId),
		Spec: pluginv1.TargetSpec_builder{
			Ref:    tref.New(gomod.basePkg, "_gomod", nil),
			Driver: htypes.Ptr("sh"),
			Config: map[string]*structpb.Value{
				"env":              p.getEnvStructpb2(),
				"runtime_pass_env": p.getRuntimePassEnvStructpb(),
				"run": hstructpb.NewStringsValue([]string{
					"go list -m -json > $OUT_JSON",
					"echo $TREE_ROOT > $OUT_ROOT",
				}),
				"out": hstructpb.NewMapStringStringValue(map[string]string{
					"json": "golist_mod.json",
					"root": "golist_mod_root",
				}),
				"in_tree":   structpb.NewBoolValue(true),
				"cache":     structpb.NewStringValue("local"),
				"hash_deps": hstructpb.NewStringsValue(files),
				"tools":     p.getGoToolStructpb(),
			},
		}.Build(),
	}.Build())
	if err != nil {
		return nil, fmt.Errorf("gomod: %w", err)
	}

	jsonArtifacts := hartifact.FindOutputs(res.GetArtifacts(), "json")
	rootArtifacts := hartifact.FindOutputs(res.GetArtifacts(), "root")

	if len(jsonArtifacts) == 0 || len(rootArtifacts) == 0 {
		return nil, connect.NewError(connect.CodeInternal, errors.New("gomodules: no output found"))
	}

	jsonArtifact := jsonArtifacts[0]
	rootArtifact := rootArtifacts[0]

	rootb, err := hartifact.FileReadAll(ctx, rootArtifact)
	if err != nil {
		return nil, err
	}

	root := strings.TrimSpace(string(rootb))

	jsonf, err := hartifact.FileReader(ctx, jsonArtifact)
	if err != nil {
		return nil, err
	}
	defer jsonf.Close()

	var modules []Module

	dec := json.NewDecoder(jsonf)
	for {
		var mod Module
		err := dec.Decode(&mod)
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return nil, err
		}

		mod.HephPackage, err = tref.DirToPackage(mod.Dir, root)
		if err != nil {
			return nil, err
		}

		modules = append(modules, mod)
	}

	return modules, nil
}
