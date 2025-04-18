package plugingo

import (
	"connectrpc.com/connect"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/internal/hartifact"
	"io"
	"path/filepath"
	"slices"
	"strings"
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

	artifacts, _, err := p.goListPkg(ctx, pkg, factors, true, false, ".")
	if err != nil {
		return nil, fmt.Errorf("go list: %w", err)
	}

	outputArtifacts := hartifact.FindOutputs(artifacts, "")

	if len(outputArtifacts) == 0 {
		return nil, connect.NewError(connect.CodeInternal, errors.New("golist: no output found"))
	}

	outputArtifact := outputArtifacts[0]

	f, err := hartifact.TarFileReader(ctx, outputArtifact)
	if err != nil {
		return nil, err
	}
	defer f.Close()

	var goPkgs []Package

	dec := json.NewDecoder(f)
	for {
		var goPkg Package
		err := dec.Decode(&goPkg)
		if err == io.EOF {
			break
		}
		if err != nil {
			return nil, err
		}

		goPkgs = append(goPkgs, goPkg)
	}

	for i, goPkg := range goPkgs {
		if rest, ok := strings.CutPrefix(goPkg.Dir, p.root); ok {
			goPkg.HephPackage = strings.TrimLeft(rest, string(filepath.Separator))
		} else {
			goPkg.IsStd = slices.ContainsFunc(stdList, func(p Package) bool {
				return p.ImportPath == goPkg.ImportPath
			})
		}

		goPkgs[i] = goPkg
	}

	slices.SortFunc(goPkgs, func(a, b Package) int {
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
	}

	return goPkg, nil
}
