package plugingo

import (
	"context"
	"errors"
	"fmt"
	"io"
	"path"

	"github.com/hephbuild/heph/internal/htypes"

	"github.com/hephbuild/heph/lib/tref"

	"github.com/goccy/go-json"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"google.golang.org/protobuf/types/known/structpb"
)

func (p *Plugin) resultStdList(ctx context.Context, factors Factors, requestId string) ([]Package, error) {
	res, err, _ := p.resultStdListMem.Do(factors, func() ([]Package, error) {
		return p.resultStdListInner(ctx, factors, requestId)
	})

	return res, err
}

func (p *Plugin) resultStdListInner(ctx context.Context, factors Factors, requestId string) ([]Package, error) {
	res, err := p.resultClient.ResultClient.Get(ctx, corev1.ResultRequest_builder{
		RequestId: htypes.Ptr(requestId),
		Ref: pluginv1.TargetRef_builder{
			Package: htypes.Ptr("@heph/go/std"),
			Name:    htypes.Ptr("install"),
			Args:    factors.Args(),
		}.Build(),
	}.Build())
	if err != nil {
		return nil, err
	}

	outputs := hartifact.FindOutputs(res.GetArtifacts(), "list")

	if len(outputs) == 0 {
		return nil, errors.New("no install artifact found")
	}

	f, err := hartifact.FileReader(ctx, outputs[0])
	if err != nil {
		return nil, err
	}
	defer f.Close()

	var packages []Package

	dec := json.NewDecoder(f)
	for {
		var goPkg Package
		err := dec.Decode(&goPkg)
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return nil, err
		}

		goPkg.HephPackage = path.Join("@heph/go/std", goPkg.ImportPath)
		goPkg.IsStd = true
		goPkg.Factors = factors

		packages = append(packages, goPkg)
	}

	return packages, nil
}

func (p *Plugin) stdInstall(ctx context.Context, factors Factors) (*pluginv1.GetResponse, error) {
	return pluginv1.GetResponse_builder{
		Spec: pluginv1.TargetSpec_builder{
			Ref: pluginv1.TargetRef_builder{
				Package: htypes.Ptr("@heph/go/std"),
				Name:    htypes.Ptr("install"),
				Args:    factors.Args(),
			}.Build(),
			Driver: htypes.Ptr("bash"),
			Config: map[string]*structpb.Value{
				"env": p.getEnvStructpb(factors, map[string]string{
					"GODEBUG": "installgoroot=all",
				}),
				"runtime_pass_env": p.getRuntimePassEnvStructpb(),
				"run": hstructpb.NewStringsValue([]string{
					"export LGOROOT=$(pwd)/goroot",
					"rm -rf $LGOROOT",
					"cp -r $(go env GOROOT) $LGOROOT",
					"export GOROOT=$LGOROOT",
					"chmod -R 777 $GOROOT",
					"go install --trimpath std",
					"go list -json std > $OUT_LIST",
				}),
				"out": hstructpb.NewMapStringStringValue(map[string]string{
					"pkg":  fmt.Sprintf("goroot/pkg/%v_%v", factors.GOOS, factors.GOARCH),
					"list": fmt.Sprintf("goroot/pkg/%v_%v/list.json", factors.GOOS, factors.GOARCH),
				}),
				"tools": p.getGoToolStructpb(),
			},
		}.Build(),
	}.Build(), nil
}

func (p *Plugin) stdLibBuild(ctx context.Context, factors Factors, goImport string) (*pluginv1.GetResponse, error) {
	return pluginv1.GetResponse_builder{
		Spec: pluginv1.TargetSpec_builder{
			Ref: pluginv1.TargetRef_builder{
				Package: htypes.Ptr(path.Join("@heph/go/std", goImport)),
				Name:    htypes.Ptr("build_lib"),
				Args:    factors.Args(),
			}.Build(),
			Driver: htypes.Ptr("bash"),
			Config: map[string]*structpb.Value{
				"env": hstructpb.NewMapStringStringValue(map[string]string{
					"GOOS":        factors.GOOS,
					"GOARCH":      factors.GOARCH,
					"GOTOOLCHAIN": "local",
				}),
				"deps": structpb.NewStringValue(tref.Format(pluginv1.TargetRef_builder{
					Package: htypes.Ptr("@heph/go/std"),
					Name:    htypes.Ptr("install"),
					Args:    factors.Args(),
				}.Build())),
				"run": hstructpb.NewStringsValue([]string{
					fmt.Sprintf("mv $WORKDIR/@heph/go/std/goroot/pkg/%v_%v/%v.a $OUT_A", factors.GOOS, factors.GOARCH, goImport),
				}),
				"out": hstructpb.NewMapStringStringValue(map[string]string{
					"a": goImport + ".a",
				}),
				"cache": structpb.NewStringValue("local"),
			},
		}.Build(),
	}.Build(), nil
}
