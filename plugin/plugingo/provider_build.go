package plugingo

import (
	"connectrpc.com/connect"
	"context"
	"fmt"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"google.golang.org/protobuf/types/known/structpb"
	"maps"
	"runtime"
	"strings"
)

func (p Provider) buildLib(ctx context.Context, ref *pluginv1.TargetRef, states []*pluginv1.ProviderState) error {

}

func (p Provider) buildTestLib(ctx context.Context, x bool, ref *pluginv1.TargetRef, states []*pluginv1.ProviderState) (*connect.Response[pluginv1.GetResponse], error) {

}

func (p Provider) buildStdLib(ctx context.Context, version string, pkg string) (*connect.Response[pluginv1.GetResponse], error) {

}

func (p Provider) buildThirdpartyLib(ctx context.Context, repo string, version string, pkg string) (*connect.Response[pluginv1.GetResponse], error) {

}

func (p Provider) buildBin(ctx context.Context, ref *pluginv1.TargetRef, states []*pluginv1.ProviderState) error {

}

func (p Provider) buildTestBin(ctx context.Context, ref *pluginv1.TargetRef, states []*pluginv1.ProviderState) (*connect.Response[pluginv1.GetResponse], error) {

}

func (p Provider) runTest(ctx context.Context, ref *pluginv1.TargetRef, states []*pluginv1.ProviderState) (*connect.Response[pluginv1.GetResponse], error) {

}

func (p Provider) factors(ref *pluginv1.TargetRef) Factors {
	args := maps.Clone(ref.GetArgs())
	if args["os"] == "" {
		args["os"] = runtime.GOOS
	}
	if args["arch"] == "" {
		args["arch"] = runtime.GOARCH
	}

	factors := Factors{
		GoVersion: "1.23.4",
		GOOS:      args["os"],
		GOARCH:    args["arch"],
		Tags:      args["tags"],
	}

	return factors
}

func (p Provider) __buildPkg(ctx context.Context, ref *pluginv1.TargetRef, states []*pluginv1.ProviderState) (*connect.Response[pluginv1.GetResponse], error) {
	modState := states[0]

	factors := p.factors(ref)

	_, isStd, err := p.getStdPkgs(ctx, factors)
	if err != nil {
		return nil, err
	}

	mods, findModule, err := p.getModules(ctx, ref.GetPackage(), factors)
	if err != nil {
		return nil, err
	}
	currentMod := mods[0]

	pkg, err := p.getPkg(ctx, ref.GetPackage(), factors)
	if err != nil {
		return nil, err
	}

	if pkg.IsCommand() {
		// build bin
	} else {
		// build lib
	}

	var goImports []string
	for _, imp := range pkg.Imports {
		if isStd(imp) {
			goImports = append(goImports, tref.Format(&tref.Ref{
				Package: fmt.Sprintf("@go/toolchain/%v/std/%v", factors.GoVersion, imp),
				Name:    "build_lib",
				Args:    factors.Args(),
			}))
		} else {
			rest, mod, ok := findModule(imp)
			if !ok {
				return nil, connect.NewError(connect.CodeInternal, fmt.Errorf("no module found for %v", imp))
			}

			// TODO: support in-tree replace directives
			if mod.Path == currentMod.Path {
				rest, _ := strings.CutPrefix(imp, mod.Path)
				rest = strings.TrimPrefix(rest, "/")

				goImports = append(goImports, tref.Format(&tref.Ref{
					Package: fmt.Sprintf("%v/%v", modState.Package, rest),
					Name:    "build_lib",
					Args:    factors.Args(),
				}))
			} else {
				goImports = append(goImports, tref.Format(&tref.Ref{
					Package: fmt.Sprintf("@go/thirdparty/%v@%v/%v", mod.Path, mod.Version, rest),
					Name:    "build_lib",
					Args:    factors.Args(),
				}))
			}
		}
	}

	return connect.NewResponse(&pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: ref.GetPackage(),
				Name:    ref.GetName(),
				Driver:  DriverName,
				Args:    factors.Args(),
			},
			Config: map[string]*structpb.Value{
				"go":         structpb.NewStringValue(fmt.Sprintf("@go/toolchain/%v:go", factors.GoVersion)),
				"pkg_list":   structpb.NewStringValue(tref.Format(pkg.Ref)),
				"go_imports": hstructpb.NewStringsValue(goImports),
			},
		},
	}), nil
}
