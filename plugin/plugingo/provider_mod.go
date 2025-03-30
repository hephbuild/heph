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
	"google.golang.org/protobuf/types/known/structpb"
	"io"
	"strings"
)

func (p Provider) resultModList(ctx context.Context, pkg string, f Factors) ([]*pluginv1.Artifact, *pluginv1.TargetRef, error) {
	res, err := p.resultClient.Get(ctx, connect.NewRequest(&corev1.ResultRequest{
		Of: &corev1.ResultRequest_Spec{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: pkg,
					Name:    "_gomodlist",
					Driver:  "exec",
					Args:    f.Args(),
				},
				Config: map[string]*structpb.Value{
					"env": hstructpb.NewMapStringStringValue(map[string]string{
						"GOOS":        f.GOOS,
						"GOARCH":      f.GOARCH,
						"CGO_ENABLED": "0",
					}),
					"run":   structpb.NewStringValue(fmt.Sprintf("go list -mod=readonly -json -tags %q -m all", f.Tags)),
					"out":   structpb.NewStringValue("modlist.json"),
					"tools": hstructpb.NewStringsValue([]string{fmt.Sprintf("//@go/%v:go", f.GoVersion)}),
					// TODO: cache based on go.mod only
				},
			},
		},
	}))
	if err != nil {
		return nil, nil, err
	}

	return res.Msg.GetArtifacts(), res.Msg.Def.Ref, nil
}

func (p Provider) getModules(ctx context.Context, pkg string, f Factors) ([]Module, func(importPath string) (string, Module, bool), error) {
	artifacts, _, err := p.resultModList(ctx, pkg, f)
	if err != nil {
		return nil, nil, err
	}

	r, err := hartifact.Reader(ctx, hartifact.FindOutputs(artifacts, "")[0])
	if err != nil {
		return nil, nil, err
	}
	defer r.Close()

	dec := json.NewDecoder(r)
	var mods []Module
	for {
		var mod Module
		err := dec.Decode(&mod) //nolint:musttag
		if err == io.EOF {
			// all done
			break
		}
		if err != nil {
			return nil, nil, err
		}

		mods = append(mods, mod)
	}

	findModule := func(importPath string) (string, Module, bool) {
		for _, mod := range mods {
			rest, ok := strings.CutPrefix(importPath, mod.Path)
			if ok {
				rest = strings.TrimPrefix(rest, "/")

				return rest, mod, true
			}
		}

		return "", Module{}, false
	}

	return mods, findModule, nil
}
