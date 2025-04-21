package plugingo

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/hsync"
	"github.com/hephbuild/heph/internal/hinstance"
	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/internal/hslices"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"golang.org/x/sync/errgroup"
	"google.golang.org/protobuf/types/known/structpb"
	"io"
	"os"
	"path/filepath"
	"slices"
	"strings"

	"connectrpc.com/connect"
	"github.com/hephbuild/heph/internal/hartifact"
)

func (p *Plugin) goListPkgResult(ctx context.Context, pkg string, factors Factors) (Package, error) {
	artifacts, _, err := p.goListPkg(ctx, pkg, factors, false, false, ".")
	if err != nil {
		return Package{}, fmt.Errorf("go list: %w", err)
	}

	outputArtifacts := hartifact.FindOutputs(artifacts, "")

	if len(outputArtifacts) == 0 {
		return Package{}, connect.NewError(connect.CodeInternal, errors.New("golist: no output found"))
	}

	outputArtifact := outputArtifacts[0]

	f, err := hartifact.TarFileReader(ctx, outputArtifact)
	if err != nil {
		return Package{}, err
	}
	defer f.Close()

	var goPkg Package
	err = json.NewDecoder(f).Decode(&goPkg.Package)
	if err != nil {
		return Package{}, err
	}
	goPkg.HephPackage = pkg

	return goPkg, nil
}

func (p *Plugin) goListDepsPkgResult(ctx context.Context, pkg string, factors Factors) ([]Package, error) {
	seen := hmaps.Sync[string, struct{}]{}
	pkgsm := hmaps.Sync[string, Package]{}

	var g errgroup.Group

	stdListRes := hsync.Go2(func() ([]Package, error) {
		return p.resultStdList(ctx, factors)
	})

	modulesRes := hsync.Go2(func() ([]Module, error) {
		return p.goModules(ctx, pkg)
	})

	g.Go(func() error {
		_, _ = stdListRes() // warmup

		return nil
	})
	g.Go(func() error {
		_, _ = modulesRes() // warmup

		return nil
	})

	var doTheMagicForStdPackage func(pkg string) error
	doTheMagicForStdPackage = func(imp string) error {
		if !seen.SetOk(imp, struct{}{}) {
			return nil
		}

		stdList, err := stdListRes()
		if err != nil {
			return fmt.Errorf("get stdlib list: %w", err)
		}

		goPkg, isStd := hslices.Find(stdList, func(p Package) bool {
			return p.ImportPath == imp
		})

		pkgsm.Set(goPkg.ImportPath, goPkg)

		if !isStd {
			return fmt.Errorf("not std, not supposed to happen")
		}

		for _, imp := range goPkg.Imports {
			err := doTheMagicForStdPackage(imp)
			if err != nil {
				return err
			}
		}

		return nil
	}

	var doTheMagicForHephPackage func(pkg string) error
	doTheMagicForHephPackage = func(pkg string) error {
		goPkg, err := p.goListPkgResult(ctx, pkg, factors)
		if err != nil {
			return fmt.Errorf("go list: %w", err)
		}

		if !pkgsm.SetOk(goPkg.ImportPath, goPkg) {
			return nil
		}

		stdList, err := stdListRes()
		if err != nil {
			return fmt.Errorf("get stdlib list: %w", err)
		}

		for _, imp := range goPkg.Imports {
			if _, ok := seen.GetOk(imp); ok {
				continue
			}

			isStd := slices.ContainsFunc(stdList, func(p Package) bool {
				return p.ImportPath == imp
			})

			if isStd {
				g.Go(func() error {
					return doTheMagicForStdPackage(imp)
				})
			} else {
				if seen.SetOk(imp, struct{}{}) {
					modules, err := modulesRes()
					if err != nil {
						return fmt.Errorf("get modules list: %w", err)
					}

					var hephPkg string
					for _, module := range modules {
						if rest, ok := strings.CutPrefix(imp, module.Path); ok {
							hephPkg = tref.JoinPackage(module.HephPackage, strings.TrimLeft(rest, "/"))
							break
						}
					}

					if hephPkg == "" {
						return fmt.Errorf("no module found")
					}

					g.Go(func() error {
						return doTheMagicForHephPackage(hephPkg)
					})
				}
			}
		}

		return nil
	}

	g.Go(func() error {
		return doTheMagicForHephPackage(pkg)
	})

	err := g.Wait()
	if err != nil {
		return nil, err
	}

	goPkgs := slices.Collect(pkgsm.Values())

	slices.SortFunc(goPkgs, func(a, b Package) int {
		if a.HephPackage == pkg {
			return -1
		}

		if b.HephPackage == pkg {
			return 1
		}

		return strings.Compare(a.ImportPath, b.ImportPath)
	})

	return goPkgs, nil
}

func (p *Plugin) goFindPkg(ctx context.Context, pkg, imp string, factors Factors) (Package, error) {
	stdList, err := p.resultStdList(ctx, factors)
	if err != nil {
		return Package{}, fmt.Errorf("get stdlib list: %w", err)
	}

	artifacts, _, err := p.goListPkg(ctx, pkg, factors, false, true, imp)
	if err != nil {
		return Package{}, fmt.Errorf("go list: %w", err)
	}

	outputArtifacts := hartifact.FindOutputs(artifacts, "")

	if len(outputArtifacts) == 0 {
		return Package{}, connect.NewError(connect.CodeInternal, errors.New("golist: no output found"))
	}

	outputArtifact := outputArtifacts[0]

	f, err := hartifact.TarFileReader(ctx, outputArtifact)
	if err != nil {
		return Package{}, err
	}
	defer f.Close()

	var goPkg Package
	err = json.NewDecoder(f).Decode(&goPkg.Package)
	if err != nil {
		return Package{}, err
	}

	stdPkg, ok := hslices.Find(stdList, func(p Package) bool {
		return p.ImportPath == goPkg.ImportPath
	})
	if ok {
		goPkg = stdPkg
	} else {
		goPkg.HephPackage, err = tref.DirToPackage(goPkg.Dir, p.root)
		if err != nil {
			return Package{}, err
		}
	}

	return goPkg, nil
}

func (p *Plugin) goModules(ctx context.Context, pkg string) ([]Module, error) {
	pkgParts := tref.SplitPackage(pkg)

	var gomod, gowork string
	var gomodPkg string

	for {
		pkg := tref.JoinPackage(pkgParts...)
		pkgDirParts := append([]string{p.root}, pkgParts...)
		pkgDir := filepath.Join(pkgDirParts...)

		if gomod == "" {
			_, err := os.Stat(filepath.Join(pkgDir, "go.mod"))
			if err == nil {
				gomod = tref.JoinPackage(pkg, "go.mod")
				gomodPkg = pkg
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

		pkgParts = pkgParts[:len(pkgParts)-2]
	}

	if gomod == "" || gomodPkg == "" {
		return nil, fmt.Errorf("no module for %v", pkg)
	}

	files := []string{
		tref.Format(&pluginv1.TargetRef{
			Package: tref.JoinPackage("@heph/file", gomod),
			Name:    "content",
		}),
	}
	if gowork != "" {
		files = append(files, tref.Format(&pluginv1.TargetRef{
			Package: tref.JoinPackage("@heph/file", gowork),
			Name:    "content",
		}))
	}

	res, err := p.resultClient.ResultClient.Get(ctx, connect.NewRequest(&corev1.ResultRequest{
		Of: &corev1.ResultRequest_Spec{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: gomodPkg,
					Name:    "_gomod",
					Driver:  "sh",
				},
				Config: map[string]*structpb.Value{
					"env": hstructpb.NewMapStringStringValue(map[string]string{
						"HEPH_HASH": hinstance.Hash(),
					}),
					"runtime_pass_env": hstructpb.NewStringsValue([]string{"HOME"}),
					"run":              structpb.NewStringValue("go list -m -json > $OUT"),
					"out":              structpb.NewStringValue("golist_mod.json"),
					"in_tree":          structpb.NewBoolValue(true),
					"cache":            structpb.NewBoolValue(true),
					"hash_deps":        hstructpb.NewStringsValue(files),
					// "tools": hstructpb.NewStringsValue([]string{fmt.Sprintf("//go_toolchain/%v:go", f.GoVersion)}),
				},
			},
		},
	}))
	if err != nil {
		return nil, fmt.Errorf("gomod: %w", err)
	}

	outputArtifacts := hartifact.FindOutputs(res.Msg.GetArtifacts(), "")

	if len(outputArtifacts) == 0 {
		return nil, connect.NewError(connect.CodeInternal, errors.New("gomodules: no output found"))
	}

	outputArtifact := outputArtifacts[0]

	f, err := hartifact.TarFileReader(ctx, outputArtifact)
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
