package plugingo

import (
	"connectrpc.com/connect"
	"context"
	"fmt"
	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"google.golang.org/protobuf/types/known/structpb"
	"slices"
	"strings"
)

func (p *Plugin) packageLib(ctx context.Context, basePkg string, goPkg Package, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	if len(goPkg.SFiles) > 0 {
		deps := map[string][]string{}

		deps["lib"] = []string{tref.Format(tref.WithOut(&pluginv1.TargetRef{
			Package: goPkg.GetHephBuildPackage(),
			Name:    "build_pure_lib",
			Args:    factors.Args(),
		}, "a"))}

		var asmDeps []string
		for _, file := range goPkg.SFiles {
			asmDeps = append(asmDeps, tref.Format(&pluginv1.TargetRef{
				Package: goPkg.GetHephBuildPackage(),
				Name:    "build_lib#asm",
				Args: hmaps.Concat(factors.Args(), map[string]string{
					"file": file,
				}),
			}))
		}

		return connect.NewResponse(&pluginv1.GetResponse{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: goPkg.GetHephBuildPackage(),
					Name:    "build_lib",
					Args:    factors.Args(),
				},
				Driver: "bash",
				Config: map[string]*structpb.Value{
					"env": hstructpb.NewMapStringStringValue(map[string]string{
						"GOOS":        factors.GOOS,
						"GOARCH":      factors.GOARCH,
						"CGO_ENABLED": "0",
					}),
					"runtime_pass_env": hstructpb.NewStringsValue([]string{"HOME"}),
					"run": hstructpb.NewStringsValue([]string{
						// This appends to $SRC_LIB, so name of input & output need to be the same.
						`go tool pack r "$SRC_LIB" $SRC_ASM`,
					}),
					"out": hstructpb.NewMapStringStringValue(map[string]string{
						"a": goPkg.Name + ".a",
					}),
					"deps": hstructpb.NewMapStringStringsValue(map[string][]string{
						"lib": {tref.Format(tref.WithOut(&pluginv1.TargetRef{
							Package: goPkg.GetHephBuildPackage(),
							Name:    "build_lib#incomplete",
							Args:    factors.Args(),
						}, "a"))},
						"asm": asmDeps,
					}),
				},
			},
		}), nil
	}

	return p.packageLibInner(ctx, basePkg, goPkg, factors, false)
}

func (p *Plugin) packageLibIncomplete(ctx context.Context, basePkg string, goPkg Package, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	return p.packageLibInner(ctx, basePkg, goPkg, factors, true)
}

func (p *Plugin) importPathToPackage(ctx context.Context, imp string) (string, error) {
	return imp, nil
}

func (p *Plugin) packageLibInner(ctx context.Context, basePkg string, goPkg Package, factors Factors, incomplete bool) (*connect.Response[pluginv1.GetResponse], error) {
	return p.packageLibInner2(ctx, "build_lib", nil, goPkg.Name, basePkg, LibPackage{
		Imports:       goPkg.Imports,
		EmbedPatterns: goPkg.EmbedPatterns,
		GoFiles:       goPkg.GoFiles,
		GoPkg:         goPkg,
		ImportPath:    goPkg.ImportPath,
	}, factors, incomplete)
}

type LibPackage struct {
	Imports       []string
	EmbedPatterns []string
	GoFiles       []string

	GoPkg      Package
	ImportPath string
}

func (p *Plugin) packageLibInner2(ctx context.Context, targetName string, extraArgs map[string]string, outName, basePkg string, goPkg LibPackage, factors Factors, incomplete bool) (*connect.Response[pluginv1.GetResponse], error) {
	if len(goPkg.GoFiles) == 0 {
		return nil, fmt.Errorf("empty go file")
	}

	c := p.newGetGoPackageCache(ctx, basePkg, factors)

	imports, err := p.goImportsToGoPkgs(ctx, goPkg.Imports, factors, c)
	if err != nil {
		return nil, err
	}

	importsm := map[string]string{}
	for i := range goPkg.Imports {
		impGoPkg := imports[i]

		if impGoPkg.ImportPath == "unsafe" {
			// ignore pseudo package
			continue
		}

		importsm[impGoPkg.ImportPath] = tref.Format(tref.WithOut(impGoPkg.GetBuildLibTargetRef(), "a"))
	}

	return p.packageLibInner3(
		ctx,
		targetName,
		outName,
		extraArgs,
		goPkg.GoPkg.GetHephBuildPackage(),
		goPkg.ImportPath,
		importsm,
		getFiles(goPkg.GoPkg, goPkg.GoFiles),
		len(goPkg.EmbedPatterns) > 0,
		factors,
		incomplete,
	)
}

func (p *Plugin) packageLibInner3(ctx context.Context, targetName, outName string, extraArgs map[string]string, buildPkg, buildImportPath string, imports map[string]string, srcRefs []string, embedCfg bool, factors Factors, incomplete bool) (*connect.Response[pluginv1.GetResponse], error) {
	deps := map[string][]string{}
	run := []string{
		`echo > importconfig`,
	}

	var i int
	for importPath, depRef := range hmaps.Sorted(imports) {
		i++

		if importPath == "unsafe" {
			// ignore pseudo package
			continue
		}

		deps[fmt.Sprintf("lib%v", i)] = []string{depRef}

		run = append(run, fmt.Sprintf(`echo "packagefile %v=${SRC_LIB%v}" >> importconfig`, importPath, i))
	}

	var extra string

	if embedCfg {
		deps["embed"] = append(deps["embed"], tref.Format(&pluginv1.TargetRef{
			Package: buildPkg,
			Name:    "embedcfg",
			Args:    factors.Args(),
		}))

		extra += " -embedcfg $SRC_EMBED"
	}

	if incomplete {
		extra += " -symabis $SRC_ABI_ABI -asmhdr $SRC_ABI_H"

		deps["abi"] = append(deps["abi"], tref.Format(&pluginv1.TargetRef{
			Package: buildPkg,
			Name:    "build_lib#abi",
			Args:    factors.Args(),
		}))
	} else {
		extra += " -complete"
	}

	deps["src"] = srcRefs

	run = append(run, fmt.Sprintf(`go tool compile -importcfg importconfig -o $OUT_A -pack -p %v %v $SRC_SRC`, buildImportPath, extra))

	name := targetName
	if incomplete {
		name += "#incomplete"
	}

	out := map[string]string{
		"a": outName + ".a",
	}
	if incomplete {
		out["h"] = "go_asm.h"
	}

	return connect.NewResponse(&pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: buildPkg,
				Name:    name,
				Args:    hmaps.Concat(factors.Args(), extraArgs),
			},
			Driver: "bash",
			Config: map[string]*structpb.Value{
				"env": hstructpb.NewMapStringStringValue(map[string]string{
					"GOOS":        factors.GOOS,
					"GOARCH":      factors.GOARCH,
					"CGO_ENABLED": "0",
				}),
				"runtime_pass_env": hstructpb.NewStringsValue([]string{"HOME"}),
				"run":              hstructpb.NewStringsValue(run),
				"out":              hstructpb.NewMapStringStringValue(out),
				"deps":             hstructpb.NewMapStringStringsValue(deps),
			},
		},
	}), nil
}

func getFiles(goPkg Package, files []string) []string {
	goPath := strings.ReplaceAll(goPkg.ImportPath, goPkg.Module.Path, "")
	goPath = strings.TrimPrefix(goPath, "/")

	var out []string
	if goPkg.Is3rdParty {
		for _, file := range files {
			out = append(out, tref.Format(&pluginv1.TargetRef{
				Package: ThirdpartyContentPackage(goPkg.Module.Path, goPkg.Module.Version, goPath),
				Name:    "content",
				Args:    map[string]string{"f": file},
			}))
		}
	} else {
		for _, file := range files {
			out = append(out, tref.FormatFile(goPkg.HephPackage, file))
		}
	}
	return out
}

func (p *Plugin) packageLibAbi(ctx context.Context, goPkg Package, factors Factors) (*connect.Response[pluginv1.GetResponse], error) {
	if len(goPkg.SFiles) == 0 {
		return nil, fmt.Errorf("no s files in package")
	}

	deps := map[string][]string{}
	for k, files := range map[string][]string{"": goPkg.SFiles, "hdr": goPkg.HFiles} {
		deps[k] = append(deps[k], getFiles(goPkg, files)...)
	}

	return connect.NewResponse(&pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: goPkg.GetHephBuildPackage(),
				Name:    "build_lib#abi",
				Args:    factors.Args(),
			},
			Driver: "bash",
			Config: map[string]*structpb.Value{
				"env": hstructpb.NewMapStringStringValue(map[string]string{
					"GOOS":        factors.GOOS,
					"GOARCH":      factors.GOARCH,
					"CGO_ENABLED": "0",
				}),
				"runtime_pass_env": hstructpb.NewStringsValue([]string{"HOME"}),
				"run": hstructpb.NewStringsValue([]string{
					"eval $(go env)",
					"touch $OUT_H",
					fmt.Sprintf(`go tool asm -I . -I $GOROOT/pkg/include -D GOOS_$GOOS -D GOARCH_$GOARCH -p %v -gensymabis -o "$OUT_ABI" $SRC`, goPkg.GetBuildImportPath()),
				}),
				"out": hstructpb.NewMapStringStringValue(map[string]string{
					"abi": goPkg.Name + ".abi",
					"h":   "go_asm.h",
				}),
				"deps": hstructpb.NewMapStringStringsValue(deps),
			},
		},
	}), nil
}

func (p *Plugin) packageLibAsm(ctx context.Context, goPkg Package, factors Factors, asmFile string) (*connect.Response[pluginv1.GetResponse], error) {
	if len(goPkg.SFiles) == 0 {
		return nil, fmt.Errorf("no s files in package")
	}

	if !slices.Contains(goPkg.SFiles, asmFile) {
		return nil, fmt.Errorf(asmFile + " not found")
	}

	return connect.NewResponse(&pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: goPkg.GetHephBuildPackage(),
				Name:    "build_lib#asm",
				Args:    hmaps.Concat(factors.Args(), map[string]string{"file": asmFile}),
			},
			Driver: "bash",
			Config: map[string]*structpb.Value{
				"env": hstructpb.NewMapStringStringValue(map[string]string{
					"GOOS":        factors.GOOS,
					"GOARCH":      factors.GOARCH,
					"CGO_ENABLED": "0",
				}),
				"runtime_pass_env": hstructpb.NewStringsValue([]string{"HOME"}),
				"run": hstructpb.NewStringsValue([]string{
					"eval $(go env)",
					"touch go_asm.h",
					fmt.Sprintf(`go tool asm -I . -I $GOROOT/pkg/include -D GOOS_$GOOS -D GOARCH_$GOARCH -p %v -o "$OUT" $SRC_ASM`, goPkg.GetBuildImportPath()),
				}),
				"out": structpb.NewStringValue(strings.ReplaceAll(asmFile, ".s", ".o")),
				"deps": hstructpb.NewMapStringStringsValue(map[string][]string{
					"lib": {tref.Format(tref.WithOut(&pluginv1.TargetRef{
						Package: goPkg.GetHephBuildPackage(),
						Name:    "build_lib#incomplete",
						Args:    factors.Args(),
					}, "a"))},
					"hdr": {tref.Format(tref.WithOut(&pluginv1.TargetRef{
						Package: goPkg.GetHephBuildPackage(),
						Name:    "build_lib#incomplete",
						Args:    factors.Args(),
					}, "h"))},
					"asm": getFiles(goPkg, []string{asmFile}),
				}),
			},
		},
	}), nil
}
