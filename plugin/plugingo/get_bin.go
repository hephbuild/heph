package plugingo

import (
	"connectrpc.com/connect"
	"context"
	"fmt"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"golang.org/x/sync/errgroup"
	"google.golang.org/protobuf/types/known/structpb"
	"maps"
	"path"
	"path/filepath"
	"slices"
	"sync"
)

func (p *Plugin) packageBinDeps(ctx context.Context, goPkg Package, factors Factors, stdList []Package) ([]string, error) {
	var g errgroup.Group
	var deps = map[string]struct{}{}
	var depsm sync.Mutex

	var analyze func(imp string) error
	analyze = func(imp string) error {
		if imp == "unsafe" {
			// ignore pseudo package
			return nil
		}

		depsm.Lock()
		_, ok := deps[imp]
		if !ok {
			deps[imp] = struct{}{}
		}
		depsm.Unlock()

		var impGoPkg Package
		for _, goPkg := range stdList {
			if goPkg.ImportPath == imp {
				impGoPkg = goPkg
			}
		}

		if impGoPkg.ImportPath == "" {
			// TODO: goListPkgResult takes a heph package
			goPkg, err := p.goListPkgResult(ctx, imp, factors)
			if err != nil {
				return err
			}

			impGoPkg = goPkg
		}

		for _, imp := range impGoPkg.Imports {
			g.Go(func() error {
				return analyze(imp)
			})
		}

		return nil
	}

	for _, imp := range goPkg.Imports {
		g.Go(func() error {
			return analyze(imp)
		})
	}

	err := g.Wait()
	if err != nil {
		return nil, err
	}

	return slices.Sorted(maps.Keys(deps)), nil
}

func (p *Plugin) packageBin(ctx context.Context, goPkg Package, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	stdList, err := p.resultStdList(ctx, factors)
	if err != nil {
		return nil, fmt.Errorf("get stdlib list: %w", err)
	}

	goDeps, err := p.packageBinDeps(ctx, goPkg, factors, stdList)
	if err != nil {
		return nil, err
	}

	deps := map[string][]string{}
	run := []string{
		`echo > importconfig`,
	}
	for i, imp := range goDeps {
		isStd := slices.ContainsFunc(stdList, func(p Package) bool {
			return p.ImportPath == imp
		})

		if isStd {
			deps[fmt.Sprintf("lib%v", i)] = []string{tref.Format(tref.WithOut(&pluginv1.TargetRef{
				Package: path.Join("@heph/go/std", imp),
				Name:    "build_lib",
				Args:    factors.Args(),
			}, "a"))}
		} else {
			deps[fmt.Sprintf("lib%v", i)] = []string{tref.Format(tref.WithOut(&pluginv1.TargetRef{
				Package: imp,
				Name:    "build_lib",
				Args:    factors.Args(),
			}, "a"))}
		}

		run = append(run, fmt.Sprintf(`echo "packagefile %v=${SRC_LIB%v}" >> importconfig`, imp, i))
	}

	deps["main"] = []string{tref.Format(tref.WithOut(&pluginv1.TargetRef{
		Package: goPkg.HephPackage,
		Name:    "build_lib",
		Args:    factors.Args(),
	}, "a"))}

	run = append(run, fmt.Sprintf(`go tool link -importcfg "importconfig" -o $OUT $SRC_MAIN`))

	return connect.NewResponse(&pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: goPkg.HephPackage,
				Name:    "build",
				Driver:  "bash",
				Args:    factors.Args(),
			},
			Config: map[string]*structpb.Value{
				"env": hstructpb.NewMapStringStringValue(map[string]string{
					"GOOS":        factors.GOOS,
					"GOARCH":      factors.GOARCH,
					"CGO_ENABLED": "0",
				}),
				"runtime_pass_env": hstructpb.NewStringsValue([]string{"HOME"}),
				"run":              hstructpb.NewStringsValue(run),
				"out":              structpb.NewStringValue(filepath.Base(goPkg.HephPackage)),
				"deps":             hstructpb.NewMapStringStringsValue(deps),
			},
		},
	}), nil
}
