package plugingo

import (
	"connectrpc.com/connect"
	"context"
	"encoding/base64"
	"fmt"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"google.golang.org/protobuf/types/known/structpb"
	"path/filepath"
)

func (p *Plugin) runTest(ctx context.Context, pkg string, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	return connect.NewResponse(&pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: pkg,
				Name:    "test",
				Args:    factors.Args(),
			},
			Driver: "bash",
			Config: map[string]*structpb.Value{
				"run": structpb.NewStringValue("$SRC"),
				"deps": structpb.NewStringValue(tref.Format(&pluginv1.TargetRef{
					Package: pkg,
					Name:    "build_test",
					Args:    factors.Args(),
				})),
			},
		},
	}), nil
}

func (p *Plugin) packageBinTest(ctx context.Context, basePkg string, goPkg Package, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	c := p.newGetGoPackageCache(ctx, basePkg, factors)

	goPkgs, err := p.goListTestDepsPkgResult(ctx, goPkg.GetHephBuildPackage(), factors, c, testmainImports)
	if err != nil {
		return nil, err
	}

	mainRef := tref.Format(tref.WithOut(&pluginv1.TargetRef{
		Package: goPkg.GetHephBuildPackage(),
		Name:    "build_testmain_lib",
		Args:    factors.Args(),
	}, "a"))

	return p.packageBinInner(ctx, "build_test", goPkg, factors, mainRef, goPkgs)
}

func (p *Plugin) packageTestLib(ctx context.Context, basePkg string, goPkg Package, factors Factors, xtest bool) (*connect.Response[pluginv1.GetResponse], error) {
	if xtest {
		return p.packageLibInner2(ctx, "build_test_lib", map[string]string{"x": "true"}, goPkg.Name+"_xtest", basePkg, LibPackage{
			Imports:       goPkg.XTestImports,
			EmbedPatterns: goPkg.XTestEmbedPatterns,
			GoFiles:       goPkg.XTestGoFiles,
			ImportPath:    goPkg.ImportPath + "_test",
			GoPkg:         goPkg,
		}, factors, false)
	}

	return p.packageLibInner2(ctx, "build_test_lib", map[string]string{"x": "false"}, goPkg.Name, basePkg, LibPackage{
		Imports:       goPkg.TestImports,
		EmbedPatterns: goPkg.TestEmbedPatterns,
		GoFiles:       goPkg.TestGoFiles,
		ImportPath:    goPkg.ImportPath,
		GoPkg:         goPkg,
	}, factors, false)
}

func (p *Plugin) generateTestMain(ctx context.Context, goPkg Package, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	var absFiles []string
	for _, file := range goPkg.TestGoFiles {
		absFiles = append(absFiles, "_test:"+filepath.Join(goPkg.Dir, file))
	}
	for _, file := range goPkg.XTestGoFiles {
		absFiles = append(absFiles, "_xtest:"+filepath.Join(goPkg.Dir, file))
	}

	analysis, err := analyzeTestMain(goPkg.ImportPath, absFiles)
	if err != nil {
		return nil, fmt.Errorf("analyze: %w", err)
	}

	testmainb, err := generateTestMain(analysis)
	if err != nil {
		return nil, fmt.Errorf("analyze: %w", err)
	}

	testmain := base64.StdEncoding.EncodeToString(testmainb)

	return connect.NewResponse(&pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: goPkg.GetHephBuildPackage(),
				Name:    "testmain",
				Args:    factors.Args(),
			},
			Driver: "bash",
			Config: map[string]*structpb.Value{
				"run": structpb.NewStringValue(fmt.Sprintf("echo %q | base64 --decode > $OUT", testmain)),
				"out": structpb.NewStringValue("testmain.go"),
			},
		},
	}), nil
}

var testmainImports = []string{"os", "reflect", "testing", "testing/internal/testdeps"}

func (p *Plugin) testMainLib(ctx context.Context, basePkg string, goPkg Package, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	importsm := map[string]string{}

	c := p.newGetGoPackageCache(ctx, basePkg, factors)

	if len(goPkg.TestGoFiles) > 0 {
		testGoPkg, err := p.getGoTestPackageFromImportPath(ctx, goPkg.ImportPath, factors, c, false)
		if err != nil {
			return nil, err
		}

		importsm[testGoPkg.ImportPath] = tref.Format(tref.WithOut(testGoPkg.GetBuildLibTargetRef(), "a"))
	}

	if len(goPkg.XTestGoFiles) > 0 {
		testGoPkg, err := p.getGoTestPackageFromImportPath(ctx, goPkg.ImportPath, factors, c, true)
		if err != nil {
			return nil, err
		}

		importsm[testGoPkg.ImportPath] = tref.Format(tref.WithOut(testGoPkg.GetBuildLibTargetRef(), "a"))
	}

	if len(importsm) == 0 {
		return nil, fmt.Errorf("this package has no tests")
	}

	goPkg, err := p.getGoTestmainPackageFromImportPath(ctx, goPkg.ImportPath, factors, c)
	if err != nil {
		return nil, err
	}

	imports, err := p.goImportsToGoPkgs(ctx, goPkg.Imports, factors, c)
	if err != nil {
		return nil, err
	}

	for i := range goPkg.Imports {
		impGoPkg := imports[i]

		if impGoPkg.ImportPath == "unsafe" {
			// ignore pseudo package
			continue
		}

		importsm[impGoPkg.ImportPath] = tref.Format(tref.WithOut(impGoPkg.GetBuildLibTargetRef(), "a"))
	}

	return p.packageLibInner3(
		ctx,
		goPkg.GetBuildLibTargetRef().Name,
		"testmain_"+goPkg.Name,
		nil,
		goPkg.GetBuildLibTargetRef().Package,
		goPkg.GetBuildImportPath(),
		importsm,
		[]string{tref.Format(&pluginv1.TargetRef{
			Package: goPkg.GetHephBuildPackage(),
			Name:    "testmain",
			Args:    factors.Args(),
		})},
		false,
		factors,
		false,
	)
}
