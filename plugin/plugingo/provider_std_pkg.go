package plugingo

import (
	"connectrpc.com/connect"
	"context"
	"encoding/json"
	"fmt"
	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"go/build"
	"google.golang.org/protobuf/types/known/structpb"
	"io"
)

func (p Provider) resultStd(ctx context.Context, f Factors) ([]*pluginv1.Artifact, error) {
	res, err := p.resultClient.Get(ctx, connect.NewRequest(&corev1.ResultRequest{
		Of: &corev1.ResultRequest_Spec{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "std",
					Driver:  "sh",
					Args:    f.Args(),
				},
				Config: map[string]*structpb.Value{
					"env": hstructpb.NewMapStringStringValue(map[string]string{
						"GODEBUG":     "installgoroot=all",
						"GOOS":        f.GOOS,
						"GOARCH":      f.GOARCH,
						"CGO_ENABLED": "0",
					}),
					"run":   structpb.NewStringValue(fmt.Sprintf("go list -mod=readonly -json -tags %q std > $OUT", f.Tags)),
					"tools": hstructpb.NewStringsValue([]string{fmt.Sprintf("//@go/toolchain/%v:go", f.GoVersion)}),
					"out":   structpb.NewStringValue("golist.json"),
				},
			},
		},
	}))
	if err != nil {
		return nil, err
	}

	return res.Msg.GetArtifacts(), nil
}

func (p Provider) getStdPkgs(ctx context.Context, f Factors) ([]build.Package, func(importPath string) bool, error) {
	artifacts, err := p.resultStd(ctx, f)
	if err != nil {
		return nil, nil, err
	}

	r, err := hartifact.Reader(ctx, hartifact.FindOutputs(artifacts, "")[0])
	if err != nil {
		return nil, nil, err
	}
	defer r.Close()

	dec := json.NewDecoder(r)
	var pkgs []build.Package
	for {
		var mod build.Package

		err := dec.Decode(&mod)
		if err == io.EOF {
			// all done
			break
		}
		if err != nil {
			return nil, nil, err
		}

		pkgs = append(pkgs, mod)
	}

	isStd := func(importPath string) bool {
		for _, pkg := range pkgs {
			if pkg.ImportPath == importPath {
				return true
			}
		}

		return false
	}

	return pkgs, isStd, nil
}
