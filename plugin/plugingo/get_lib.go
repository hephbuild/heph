package plugingo

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"google.golang.org/protobuf/types/known/structpb"
	"path"
	"slices"
	"strings"
)

func (p *Plugin) packageLib(ctx context.Context, basePkg string, _goPkg Package, factors Factors, mode, requestId string) (*pluginv1.GetResponse, error) {
	goPkg, err := p.libGoPkg(ctx, _goPkg, mode)
	if err != nil {
		return nil, err
	}

	if mode != ModeXTest && len(goPkg.GoPkg.SFiles) > 0 {
		var asmDeps []string
		for _, file := range goPkg.GoPkg.SFiles {
			asmDeps = append(asmDeps, tref.Format(&pluginv1.TargetRef{
				Package: goPkg.LibTargetRef.Package,
				Name:    "build_lib#asm",
				Args: hmaps.Concat(goPkg.LibTargetRef.Args, map[string]string{
					"file": file,
				}),
			}))
		}

		return &pluginv1.GetResponse{
			Spec: &pluginv1.TargetSpec{
				Ref:    goPkg.LibTargetRef,
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
						`mv $SRC_LIB $OUT_A`,
					}),
					"out": hstructpb.NewMapStringStringValue(map[string]string{
						"a": goPkg.Name + ".a",
					}),
					"deps": hstructpb.NewMapStringStringsValue(map[string][]string{
						"lib": {tref.Format(tref.WithOut(&pluginv1.TargetRef{
							Package: goPkg.LibTargetRef.Package,
							Name:    "build_lib#incomplete",
							Args:    goPkg.LibTargetRef.Args,
						}, "a"))},
						"asm": asmDeps,
					}),
				},
			},
		}, nil
	}

	return p.packageLibInner(ctx, basePkg, goPkg, factors, false, requestId)
}

func (p *Plugin) packageLibIncomplete(ctx context.Context, basePkg string, _goPkg Package, factors Factors, mode, requestId string) (*pluginv1.GetResponse, error) {
	goPkg, err := p.libGoPkg(ctx, _goPkg, mode)
	if err != nil {
		return nil, err
	}

	return p.packageLibInner(ctx, basePkg, goPkg, factors, true, requestId)
}

func (p *Plugin) packageLibInner(ctx context.Context, basePkg string, goPkg LibPackage, factors Factors, incomplete bool, requestId string) (*pluginv1.GetResponse, error) {
	if len(goPkg.GoFiles) == 0 {
		return nil, fmt.Errorf("empty go file")
	}

	c := p.newGetGoPackageCache(ctx, basePkg, factors, requestId)

	imports, err := p.goImportsToGoPkgs(ctx, goPkg.Imports, factors, c, requestId)
	if err != nil {
		return nil, err
	}

	importsm := map[string]string{}
	for _, impGoPkg := range imports {
		if impGoPkg.ImportPath == "unsafe" {
			// ignore pseudo package
			continue
		}

		importsm[impGoPkg.ImportPath] = tref.Format(tref.WithOut(impGoPkg.GetBuildLibTargetRef(ModeNormal), "a"))
	}

	return p.packageLibInner3(
		ctx,
		"build_lib",
		goPkg,
		importsm,
		getFiles(goPkg.GoPkg, goPkg.GoFiles),
		factors,
		incomplete,
	)
}

type LibPackage struct {
	Mode string

	Imports       []string
	EmbedPatterns []string
	GoFiles       []string

	GoPkg        Package
	ImportPath   string
	Name         string
	LibTargetRef *pluginv1.TargetRef
}

func (p *Plugin) libGoPkg(ctx context.Context, goPkg Package, mode string) (LibPackage, error) {
	switch mode {
	case ModeNormal:
		return LibPackage{
			Mode:          ModeNormal,
			Imports:       goPkg.Imports,
			EmbedPatterns: goPkg.EmbedPatterns,
			GoFiles:       goPkg.GoFiles,
			ImportPath:    goPkg.GetBuildImportPath(true),
			Name:          goPkg.Name,
			GoPkg:         goPkg,
			LibTargetRef:  goPkg.GetBuildLibTargetRef(ModeNormal),
		}, nil
	case ModeTest:
		return LibPackage{
			Mode:          ModeTest,
			Imports:       append(goPkg.Imports, goPkg.TestImports...),
			EmbedPatterns: append(goPkg.EmbedPatterns, goPkg.TestEmbedPatterns...),
			GoFiles:       append(goPkg.GoFiles, goPkg.TestGoFiles...),
			ImportPath:    goPkg.GetBuildImportPath(false),
			Name:          goPkg.Name,
			GoPkg:         goPkg,
			LibTargetRef:  goPkg.GetBuildLibTargetRef(ModeTest),
		}, nil
	case ModeXTest:
		return LibPackage{
			Mode:          ModeXTest,
			Imports:       goPkg.XTestImports,
			EmbedPatterns: goPkg.XTestEmbedPatterns,
			GoFiles:       goPkg.XTestGoFiles,
			ImportPath:    goPkg.GetBuildImportPath(false) + "_test",
			Name:          goPkg.Name + "_test",
			GoPkg:         goPkg,
			LibTargetRef:  goPkg.GetBuildLibTargetRef(ModeXTest),
		}, nil
	default:
		return LibPackage{}, fmt.Errorf("unknown mode %q", mode)
	}
}

const (
	ModeNormal = ""
	ModeTest   = "test"
	ModeXTest  = "xtest"
)

func (p *Plugin) packageLibInner3(ctx context.Context, name string, goPkg LibPackage, imports map[string]string, srcRefs []string, factors Factors, incomplete bool) (*pluginv1.GetResponse, error) {
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

	if len(goPkg.EmbedPatterns) > 0 {
		deps["embed"] = append(deps["embed"], tref.Format(&pluginv1.TargetRef{
			Package: goPkg.LibTargetRef.Package,
			Name:    "embedcfg",
			Args:    goPkg.LibTargetRef.Args,
		}))

		extra += " -embedcfg $SRC_EMBED"
	}

	if incomplete {
		extra += " -symabis $SRC_ABI_ABI -asmhdr $SRC_ABI_H"

		deps["abi"] = append(deps["abi"], tref.Format(&pluginv1.TargetRef{
			Package: goPkg.LibTargetRef.Package,
			Name:    "build_lib#abi",
			Args:    goPkg.LibTargetRef.Args,
		}))
	} else {
		extra += " -complete"
	}

	deps["src"] = srcRefs

	run = append(run, fmt.Sprintf(`go tool compile -importcfg importconfig -o $OUT_A -pack -p %v %v $SRC_SRC`, goPkg.ImportPath, extra))

	outFile := goPkg.Name + ".a"
	if goPkg.Mode != "" {
		outFile = goPkg.Name + "_" + goPkg.Mode + ".a"
	}
	if incomplete {
		name += "#incomplete"
		outFile = "inc_" + outFile
	}

	out := map[string]string{
		"a": outFile,
	}
	if incomplete {
		out["h"] = "go_asm.h"
	}

	return &pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: goPkg.LibTargetRef.Package,
				Name:    name,
				Args:    goPkg.LibTargetRef.Args,
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
	}, nil
}

func getFiles(goPkg Package, files []string) []string {
	goPath := strings.ReplaceAll(goPkg.ImportPath, goPkg.Module.Path, "")
	goPath = strings.TrimPrefix(goPath, "/")

	var out []string
	if goPkg.Is3rdParty {
		if len(files) == 0 {
			return nil
		}

		var filters []string
		for _, file := range files {
			filters = append(filters, path.Join(ThirdpartyContentPackage(goPkg.Module.Path, goPkg.Module.Version, ""), goPath, file))
		}

		out = append(out, tref.Format(tref.WithFilters(tref.WithOut(&pluginv1.TargetRef{
			Package: ThirdpartyContentPackage(goPkg.Module.Path, goPkg.Module.Version, ""),
			Name:    "download",
		}, ""), filters)))
	} else {
		for _, file := range files {
			out = append(out, tref.FormatFile(goPkg.HephPackage, file))
		}
	}
	return out
}

func (p *Plugin) packageLibAbi(ctx context.Context, _goPkg Package, factors Factors, mode string) (*pluginv1.GetResponse, error) {
	if len(_goPkg.SFiles) == 0 {
		return nil, fmt.Errorf("no s files in package")
	}

	goPkg, err := p.libGoPkg(ctx, _goPkg, mode)
	if err != nil {
		return nil, err
	}

	deps := map[string][]string{}
	for k, files := range map[string][]string{"": goPkg.GoPkg.SFiles, "hdr": goPkg.GoPkg.HFiles} {
		deps[k] = append(deps[k], getFiles(goPkg.GoPkg, files)...)
	}

	args := factors.Args()
	if goPkg.Mode != ModeNormal {
		args["mode"] = goPkg.Mode
	}

	return &pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: goPkg.GoPkg.GetHephBuildPackage(),
				Name:    "build_lib#abi",
				Args:    args,
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
					fmt.Sprintf(`go tool asm -I . -I $GOROOT/pkg/include -D GOOS_$GOOS -D GOARCH_$GOARCH -p %v -gensymabis -o "$OUT_ABI" $SRC`, goPkg.ImportPath),
				}),
				"out": hstructpb.NewMapStringStringValue(map[string]string{
					"abi": goPkg.Name + ".abi",
					"h":   "go_asm.h",
				}),
				"deps": hstructpb.NewMapStringStringsValue(deps),
			},
		},
	}, nil
}

func (p *Plugin) packageLibAsm(ctx context.Context, _goPkg Package, factors Factors, asmFile string, mode string) (*pluginv1.GetResponse, error) {
	if len(_goPkg.SFiles) == 0 {
		return nil, fmt.Errorf("no s files in package")
	}

	goPkg, err := p.libGoPkg(ctx, _goPkg, mode)
	if err != nil {
		return nil, err
	}

	if !slices.Contains(goPkg.GoPkg.SFiles, asmFile) {
		return nil, fmt.Errorf(asmFile + " not found")
	}

	return &pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: goPkg.LibTargetRef.Package,
				Name:    "build_lib#asm",
				Args:    hmaps.Concat(goPkg.LibTargetRef.Args, map[string]string{"file": asmFile}),
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
					fmt.Sprintf(`go tool asm -I . -I $GOROOT/pkg/include -D GOOS_$GOOS -D GOARCH_$GOARCH -p %v -o "$OUT" $SRC_ASM`, goPkg.ImportPath),
				}),
				"out": structpb.NewStringValue(strings.ReplaceAll(asmFile, ".s", ".o")),
				"deps": hstructpb.NewMapStringStringsValue(map[string][]string{
					"lib": {tref.Format(tref.WithOut(&pluginv1.TargetRef{
						Package: goPkg.LibTargetRef.Package,
						Name:    "build_lib#incomplete",
						Args:    goPkg.LibTargetRef.Args,
					}, "a"))},
					"hdr": {tref.Format(tref.WithOut(&pluginv1.TargetRef{
						Package: goPkg.LibTargetRef.Package,
						Name:    "build_lib#incomplete",
						Args:    goPkg.LibTargetRef.Args,
					}, "h"))},
					"asm": getFiles(goPkg.GoPkg, []string{asmFile}),
				}),
			},
		},
	}, nil
}
