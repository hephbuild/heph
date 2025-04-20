package plugingo

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/hephbuild/heph/internal/hslices"
	"golang.org/x/sync/errgroup"
	"path"
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
	goPkg.HephPackage = pkg
	err = json.NewDecoder(f).Decode(&goPkg.Package)
	if err != nil {
		return Package{}, err
	}

	return goPkg, nil
}

func (p *Plugin) goListDepsPkgResult(ctx context.Context, pkg string, factors Factors) ([]Package, error) {
	stdList, err := p.resultStdList(ctx, factors)
	if err != nil {
		return nil, fmt.Errorf("get stdlib list: %w", err)
	}

	seen := hmaps.Sync[string, Package]{}

	var g errgroup.Group

	var doTheMagicForStdPackage func(pkg string) error
	doTheMagicForStdPackage = func(imp string) error {
		if _, ok := seen.GetOk(imp); ok {
			return nil
		}

		goPkg, isStd := hslices.Find(stdList, func(p Package) bool {
			return p.ImportPath == imp
		})

		seen.Set(imp, goPkg)

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

		goPkg.HephPackage = pkg

		seen.Set(goPkg.ImportPath, goPkg)

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
				hephPkg := strings.ReplaceAll(imp, "mod-simple", "go/mod-simple")

				if seen.SetOk(imp, Package{}) {
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

	err = g.Wait()
	if err != nil {
		return nil, err
	}

	goPkgs := slices.Collect(seen.Values())

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

	if rest, ok := strings.CutPrefix(goPkg.Dir, p.root); ok {
		goPkg.HephPackage = strings.TrimLeft(rest, string(filepath.Separator))
	} else {
		goPkg.IsStd = slices.ContainsFunc(stdList, func(p Package) bool {
			return p.ImportPath == goPkg.ImportPath
		})
		if goPkg.IsStd {
			goPkg.HephPackage = path.Join("@heph/go/std", goPkg.ImportPath)
		}

		if !goPkg.IsStd {
			return Package{}, fmt.Errorf("package cannot resolve: %#v", goPkg)
		}
	}

	return goPkg, nil
}
