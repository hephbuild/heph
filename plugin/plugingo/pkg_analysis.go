package plugingo

import (
	"connectrpc.com/connect"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/internal/hartifact"
)

func (p *Plugin) goListPkgResult(ctx context.Context, pkg string, factors Factors) (Package, error) {
	artifacts, _, err := p.goListPkg(ctx, pkg, factors) // for a single factor
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
