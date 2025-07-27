package plugingo

import (
	"context"
	"encoding/base64"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/internal/htypes"
	"path/filepath"

	"github.com/hephbuild/heph/lib/tref"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"google.golang.org/protobuf/types/known/structpb"
)

func (p *Plugin) runTest(ctx context.Context, goPkg Package, factors Factors) (*pluginv1.GetResponse, error) {
	labels := []string{"test", "go-test"}
	if goPkg.IsStd || goPkg.Is3rdParty {
		labels = nil
	}

	return pluginv1.GetResponse_builder{
		Spec: pluginv1.TargetSpec_builder{
			Ref: pluginv1.TargetRef_builder{
				Package: htypes.Ptr(goPkg.GetHephBuildPackage()),
				Name:    htypes.Ptr("test"),
				Args:    factors.Args(),
			}.Build(),
			Driver: htypes.Ptr("bash"),
			Config: map[string]*structpb.Value{
				"run": structpb.NewStringValue("$SRC"),
				"deps": structpb.NewStringValue(tref.Format(pluginv1.TargetRef_builder{
					Package: htypes.Ptr(goPkg.GetHephBuildPackage()),
					Name:    htypes.Ptr("build_test"),
					Args:    factors.Args(),
				}.Build())),
			},
			Labels: labels,
		}.Build(),
	}.Build(), nil
}

func (p *Plugin) packageBinTest(ctx context.Context, basePkg string, goPkg Package, factors Factors, requestId string) (*pluginv1.GetResponse, error) {
	c := p.newGetGoPackageCache(ctx, basePkg, factors, requestId)

	goPkgs, err := p.goListTestDepsPkgResult(ctx, goPkg.GetHephBuildPackage(), factors, c, testmainImports, requestId)
	if err != nil {
		return nil, fmt.Errorf("go list testdeps: %w", err)
	}

	libGoPkg, err := p.getGoTestmainPackageFromImportPath(ctx, goPkg.ImportPath, factors, c, requestId)
	if err != nil {
		return nil, err
	}

	mainRef := tref.Format(tref.WithOut(libGoPkg.LibTargetRef, "a"))

	return p.packageBinInner(ctx, "build_test", goPkg, factors, mainRef, goPkgs)
}

func (p *Plugin) generateTestMain(ctx context.Context, goPkg Package, factors Factors) (*pluginv1.GetResponse, error) {
	absFiles := make([]string, 0, len(goPkg.TestGoFiles)+len(goPkg.XTestGoFiles))
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

	return pluginv1.GetResponse_builder{
		Spec: pluginv1.TargetSpec_builder{
			Ref: pluginv1.TargetRef_builder{
				Package: htypes.Ptr(goPkg.GetHephBuildPackage()),
				Name:    htypes.Ptr("testmain"),
				Args:    factors.Args(),
			}.Build(),
			Driver: htypes.Ptr("bash"),
			Config: map[string]*structpb.Value{
				"run": structpb.NewStringValue(fmt.Sprintf("echo %q | base64 --decode > $OUT", testmain)),
				"out": structpb.NewStringValue("testmain.go"),
			},
		}.Build(),
	}.Build(), nil
}

var testmainImports = []string{"os", "reflect", "testing", "testing/internal/testdeps"}

func (p *Plugin) testMainLib(ctx context.Context, basePkg string, _goPkg Package, factors Factors, requestId string) (*pluginv1.GetResponse, error) {
	importsm := map[string]string{}

	c := p.newGetGoPackageCache(ctx, basePkg, factors, requestId)

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
		return nil, errors.New("this package has no tests")
	}

	goPkg, err := p.getGoTestmainPackageFromImportPath(ctx, _goPkg.ImportPath, factors, c, requestId)
	if err != nil {
		return nil, err
	}

	imports, err := p.goImportsToGoPkgs(ctx, goPkg.Imports, factors, c, requestId)
	if err != nil {
		return nil, err
	}

	for _, impGoPkg := range imports {
		if impGoPkg.ImportPath == unsafePkgName {
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
		[]string{tref.Format(pluginv1.TargetRef_builder{
			Package: htypes.Ptr(goPkg.GoPkg.GetHephBuildPackage()),
			Name:    htypes.Ptr("testmain"),
			Args:    factors.Args(),
		}.Build())},
		factors,
		false,
	)
}
