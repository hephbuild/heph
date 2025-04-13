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
	"github.com/hephbuild/heph/plugin/tref"
	"google.golang.org/protobuf/types/known/structpb"
	"io"
	"path"
)

func (p *Plugin) resultStdList(ctx context.Context, factors Factors) ([]Package, error) {
	res, err := p.resultClient.ResultClient.Get(ctx, connect.NewRequest(&corev1.ResultRequest{
		Of: &corev1.ResultRequest_Ref{
			Ref: &pluginv1.TargetRef{
				Package: "@heph/go/std",
				Name:    "install",
				Driver:  "bash",
				Args:    factors.Args(),
			},
		},
	}))
	if err != nil {
		return nil, err
	}

	outputs := hartifact.FindOutputs(res.Msg.Artifacts, "list")

	if len(outputs) == 0 {
		return nil, fmt.Errorf("no install artifact found")
	}

	f, err := hartifact.TarFileReader(ctx, outputs[0])
	if err != nil {
		return nil, err
	}
	defer f.Close()

	var packages []Package

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

		packages = append(packages, goPkg)
	}

	return packages, nil
}

func (p *Plugin) stdInstall(ctx context.Context, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	return connect.NewResponse(&pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: "@heph/go/std",
				Name:    "install",
				Driver:  "bash",
				Args:    factors.Args(),
			},
			Config: map[string]*structpb.Value{
				"env": hstructpb.NewMapStringStringValue(map[string]string{
					"GOOS":        factors.GOOS,
					"GOARCH":      factors.GOARCH,
					"CGO_ENABLED": "0",
					"GODEBUG":     "installgoroot=all",
				}),
				"runtime_pass_env": hstructpb.NewStringsValue([]string{"HOME"}),
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
			},
		},
	}), nil
}

func (p *Plugin) stdLibBuild(ctx context.Context, factors Factors, goImport string) (*connect.Response[pluginv1.GetResponse], error) {
	return connect.NewResponse(&pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: path.Join("@heph/go/std", goImport),
				Name:    "build_lib",
				Driver:  "bash",
				Args:    factors.Args(),
			},
			Config: map[string]*structpb.Value{
				"env": hstructpb.NewMapStringStringValue(map[string]string{
					"GOOS":   factors.GOOS,
					"GOARCH": factors.GOARCH,
				}),
				"deps": structpb.NewStringValue(tref.Format(&pluginv1.TargetRef{
					Package: "@heph/go/std",
					Name:    "install",
					Args:    factors.Args(),
				})),
				"run": hstructpb.NewStringsValue([]string{
					fmt.Sprintf("mv $WORKDIR/@heph/go/std/goroot/pkg/%v_%v/%v.a $OUT_A", factors.GOOS, factors.GOARCH, goImport),
				}),
				"out": hstructpb.NewMapStringStringValue(map[string]string{
					"a": goImport + ".a",
				}),
			},
		},
	}), nil
}
