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

func (p *Plugin) runTest(ctx context.Context, goPkg Package, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	labels := []string{"test", "go-test"}
	if goPkg.IsStd || goPkg.Is3rdParty {
		labels = nil
	}

	return connect.NewResponse(&pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: goPkg.GetHephBuildPackage(),
				Name:    "test",
				Args:    factors.Args(),
			},
			Driver: "bash",
			Config: map[string]*structpb.Value{
				"run": structpb.NewStringValue("$SRC"),
				"deps": structpb.NewStringValue(tref.Format(&pluginv1.TargetRef{
					Package: goPkg.GetHephBuildPackage(),
					Name:    "build_test",
					Args:    factors.Args(),
				})),
			},
			Labels: labels,
		},
	}), nil
}

func (p *Plugin) packageBinTest(ctx context.Context, basePkg string, goPkg Package, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	c := p.newGetGoPackageCache(ctx, basePkg, factors)

	goPkgs, err := p.goListTestDepsPkgResult(ctx, goPkg.GetHephBuildPackage(), factors, c, testmainImports)
	if err != nil {
		return nil, fmt.Errorf("go list testdeps: %w", err)
	}

	libGoPkg, err := p.getGoTestmainPackageFromImportPath(ctx, goPkg.ImportPath, factors, c)
	if err != nil {
		return nil, err
	}

	mainRef := tref.Format(tref.WithOut(libGoPkg.LibTargetRef, "a"))

	return p.packageBinInner(ctx, "build_test", goPkg, factors, mainRef, goPkgs)
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

func (p *Plugin) testMainLib(ctx context.Context, basePkg string, _goPkg Package, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	importsm := map[string]string{}

	c := p.newGetGoPackageCache(ctx, basePkg, factors)

	if len(_goPkg.TestGoFiles) > 0 {
		testGoPkg, err := p.libGoPkg(ctx, _goPkg, ModeTest)
		if err != nil {
			return nil, err
		}

		importsm[testGoPkg.ImportPath] = tref.Format(tref.WithOut(testGoPkg.LibTargetRef, "a"))
	}

	if len(_goPkg.XTestGoFiles) > 0 {
		testGoPkg, err := p.libGoPkg(ctx, _goPkg, ModeXTest)
		if err != nil {
			return nil, err
		}

		importsm[testGoPkg.ImportPath] = tref.Format(tref.WithOut(testGoPkg.LibTargetRef, "a"))
	}

	if len(importsm) == 0 {
		return nil, fmt.Errorf("this package has no tests")
	}

	goPkg, err := p.getGoTestmainPackageFromImportPath(ctx, _goPkg.ImportPath, factors, c)
	if err != nil {
		return nil, err
	}

	imports, err := p.goImportsToGoPkgs(ctx, goPkg.Imports, factors, c)
	if err != nil {
		return nil, err
	}

	for _, impGoPkg := range imports {
		if impGoPkg.ImportPath == "unsafe" {
			// ignore pseudo package
			continue
		}

		importsm[impGoPkg.ImportPath] = tref.Format(tref.WithOut(impGoPkg.GetBuildLibTargetRef(ModeNormal), "a"))
	}

	return p.packageLibInner3(
		ctx,
		"build_testmain_lib",
		goPkg,
		importsm,
		[]string{tref.Format(&pluginv1.TargetRef{
			Package: goPkg.GoPkg.GetHephBuildPackage(),
			Name:    "testmain",
			Args:    factors.Args(),
		})},
		factors,
		false,
	)
}
