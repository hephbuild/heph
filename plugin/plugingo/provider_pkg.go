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
)

func (p Provider) resultPkg(ctx context.Context, pkg string, f Factors) ([]*pluginv1.Artifact, *pluginv1.TargetRef, error) {
	res, err := p.resultClient.Get(ctx, connect.NewRequest(&corev1.ResultRequest{
		Of: &corev1.ResultRequest_Spec{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: pkg,
					Name:    "_golist",
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
					"run":   structpb.NewStringValue(fmt.Sprintf("go list -mod=readonly -json -tags %q . > $OUT", f.Tags)),
					"out":   structpb.NewStringValue("golist.json"),
					"tools": hstructpb.NewStringsValue([]string{fmt.Sprintf("//@go/%v:go", f.GoVersion)}),
					// TODO: cache based on go.mod
				},
			},
		},
	}))
	if err != nil {
		return nil, nil, err
	}

	return res.Msg.GetArtifacts(), res.Msg.Def.Ref, nil
}

func (p Provider) getPkg(ctx context.Context, pkg string, f Factors) (*Package, error) {
	artifacts, ref, err := p.resultPkg(ctx, pkg, f)
	if err != nil {
		return nil, err
	}

	r, err := hartifact.Reader(ctx, hartifact.FindOutputs(artifacts, "")[0])
	if err != nil {
		return nil, err
	}
	defer r.Close()

	var gopkg build.Package
	err = json.NewDecoder(r).Decode(&gopkg)
	if err != nil {
		return nil, err
	}

	return &Package{
		Ref:     ref,
		Package: gopkg,
	}, nil
}
