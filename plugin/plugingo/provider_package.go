package plugingo

import (
	"connectrpc.com/connect"
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"google.golang.org/protobuf/types/known/structpb"
	"path"
	"runtime"
)

type Stream struct {
	*connect.ServerStream[pluginv1.GetSpecsResponse]
}

func (s Stream) Send(spec *pluginv1.TargetSpec) (*pluginv1.TargetRef, error) {
	err := s.ServerStream.Send(&pluginv1.GetSpecsResponse{
		Of: &pluginv1.GetSpecsResponse_Spec{
			Spec: spec,
		},
	})

	return spec.Ref, err
}

func serializeRefs(refs []*pluginv1.TargetRef) []string {
	var srefs []string
	for _, ref := range refs {
		srefs = append(srefs, tref.Format(ref))
	}

	return srefs
}

func (p Provider) compilePackage(
	ctx context.Context,
	stream Stream,
	name string,
	pkg Package,
	factors Factors,
	libs []*pluginv1.TargetRef,
	complete bool,
	abi *pluginv1.TargetRef,
) (*pluginv1.TargetRef, error) {
	pkgName := path.Base(pkg.ImportPath)
	refPkg := "" // TODO

	if len(pkg.SFiles) > 0 {
		abiRef, err := stream.Send(&pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: refPkg,
				Name:    name + "#abi",
				Driver:  "sh",
				Args:    factors.Args(),
			},
			Config: map[string]*structpb.Value{
				"run": hstructpb.NewStringsValue([]string{
					`touch $OUT_H`,
					fmt.Sprintf(`go tool asm -I . -I $GOROOT/pkg/include -trimpath "$ROOT;$GO_OUTDIR" -D GOOS_$GOOS -D GOARCH_$GOARCH -p %q -gensymabis -o "$OUT_SYMABIS" $SRC`, pkg.ImportPath),
				}),
				"tools": structpb.NewStringValue(fmt.Sprintf("@go/toolchain/%v:go", factors.GoVersion)),
				"out": hstructpb.NewMapStringStringValue(map[string]string{
					"symabis": pkgName + ".asm",
					"h":       pkgName + ".asm.h",
				}),
				"deps": hstructpb.NewMapStringStringsValue(map[string][]string{
					"lib": serializeRefs(libs),
				}),
			},
		})
		if err != nil {
			return nil, err
		}

		// TODO: remove s_files
		libRef, err := p.compilePackage(ctx, stream, name+"#lib", pkg, factors, libs, false, abiRef)
		if err != nil {
			return nil, err
		}

		var asmDeps []*pluginv1.TargetRef
		for _, sfile := range pkg.SFiles {
			sfileRef, err := stream.Send(&pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: refPkg,
					Name:    name + "#asm_" + path.Base(sfile),
					Driver:  "sh",
					Args:    factors.Args(),
				},
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{
						fmt.Sprintf(`go tool asm -I . -I $GOROOT/pkg/include -trimpath "$ROOT;$GO_OUTDIR" -D GOOS_$GOOS -D GOARCH_$GOARCH -p %q -o "$OUT" $SRC_ASM`, pkg.ImportPath),
					}),
					"out": hstructpb.NewMapStringStringValue(map[string]string{
						"symabis": pkgName + ".asm",
						"h":       pkgName + ".asm.h",
					}),
					"tools": structpb.NewStringValue(fmt.Sprintf("@go/toolchain/%v:go", factors.GoVersion)),
					"deps": hstructpb.NewMapStringStringsValue(map[string][]string{
						"lib": {tref.Format(tref.WithOut(libRef, "a"))},
						"asm": serializeRefs(asmDeps),
					}),
				},
			})
			if err != nil {
				return nil, err
			}

			asmDeps = append(asmDeps, sfileRef)
		}

		return stream.Send(&pluginv1.TargetSpec{
			Ref: &pluginv1.TargetRef{
				Package: refPkg,
				Name:    name,
				Driver:  "sh",
				Args:    factors.Args(),
			},
			Config: map[string]*structpb.Value{
				"run": hstructpb.NewStringsValue([]string{
					`go tool pack r "$SRC_LIB" $SRC_ASM`,
					fmt.Sprintf(`echo "packagefile %v=$OUT_A" > $SANDBOX/$OUT_IMPORTCFG`, pkg.ImportPath),
				}),
				"tools": structpb.NewStringValue(fmt.Sprintf("@go/toolchain/%v:go", factors.GoVersion)),
				"out": hstructpb.NewMapStringStringValue(map[string]string{
					"a":         pkgName + ".a",
					"importcfg": pkgName + ".importcfg",
				}),
				"deps": hstructpb.NewMapStringStringsValue(map[string][]string{
					"lib": serializeRefs(libs),
				}),
			},
		})
	}

	deps := map[string][]string{}

	var extra string
	if abi != nil {
		extra += " -symabis $SRC_ABI -asmhdr $SRC_ABI_H"
	}

	//if embed_cfg {
	//	extra += " -embedcfg $SRC_EMBED"
	//}

	if complete {
		extra += " -complete"
	}

	xargs := "-s"
	if true { // is darwin
		xargs = "-S"
	}

	return stream.Send(&pluginv1.TargetSpec{
		Ref: &pluginv1.TargetRef{
			Package: refPkg,
			Name:    name,
			Driver:  "exec",
			Args:    factors.Args(),
		},
		Config: map[string]*structpb.Value{
			"run": hstructpb.NewStringsValue([]string{
				fmt.Sprintf(`find "$SANDBOX" -name "*.importcfg" | xargs %v 100000 -I[] cat [] | sed -e "s:=:=$SANDBOX/:" | sort -u > $SANDBOX/importconfig`, xargs),
				fmt.Sprintf(`go tool compile -importcfg $SANDBOX/importconfig -trimpath "$ROOT;$GO_OUTDIR" -o $SANDBOX/$OUT_A -pack %v $SRC_SRC`, extra),
				fmt.Sprintf(`echo "packagefile %v=$OUT_A" > $SANDBOX/$OUT_IMPORTCFG`, pkg.ImportPath),
			}),
			"tools": structpb.NewStringValue(fmt.Sprintf("@go/toolchain/%v:go", factors.GoVersion)),
			"out": hstructpb.NewMapStringStringValue(map[string]string{
				"a":         pkgName + ".a",
				"importcfg": pkgName + ".importcfg",
			}),
			"deps": hstructpb.NewMapStringStringsValue(map[string][]string{
				"lib": serializeRefs(libs),
			}),
		},
	})
}

func (p Provider) GetSpecs(ctx context.Context, req *connect.Request[pluginv1.GetSpecsRequest], rpcStream *connect.ServerStream[pluginv1.GetSpecsResponse]) error {
	factors := Factors{
		GoVersion: "1.23.4",
		GOOS:      req.Msg.Ref.Args["os"],
		GOARCH:    req.Msg.Ref.Args["arch"],
		Tags:      req.Msg.Ref.Args["tags"],
	}
	if factors.GOOS == "" {
		factors.GOOS = runtime.GOOS
	}
	if factors.GOARCH == "" {
		factors.GOARCH = runtime.GOARCH
	}
	stream := Stream{rpcStream}

	hpackage := req.Msg.Ref.Package
	hname := req.Msg.Ref.Name
	if m, ok := matchThirdparty.Match(hpackage); ok {
		repo := m["repo"]
		version := m["version"]
		pkg := m["package"]

		switch hname {
		case "download":
			// go mod download
		case "build_lib":
			return p.buildLib(ctx, stream, req.Msg.GetRef(), req.Msg.States)
		case "build":
			return p.buildBin(ctx, stream, req.Msg.GetRef(), req.Msg.States)
		}
	} else if m, ok := matchGo.Match(hpackage); ok {
		version := m["version"]

		_ = version
		panic("implement me: " + hpackage)

		switch hname {
		case "toolchain":
			// download go toolchain
		case "go":
			// return go bin
		}
	} else if m, ok := matchStd.Match(hpackage); ok {
		version := m["version"]
		pkg := m["package"]

		switch hname {
		case "build_lib":
			return p.buildStdLib(ctx, version, pkg)
		}
	} else {
		switch hname {
		case "build":
			return p.buildBin(ctx, req.Msg.GetRef(), req.Msg.States)
		case "build_test_lib":
			return p.buildTestLib(ctx, false, req.Msg.GetRef(), req.Msg.States)
		case "build_xtest_lib":
			return p.buildTestLib(ctx, true, req.Msg.GetRef(), req.Msg.States)
		case "build_test":
			return p.buildTestBin(ctx, req.Msg.GetRef(), req.Msg.States)
		case "test":
			return p.runTest(ctx, req.Msg.GetRef(), req.Msg.States)
		case "build_lib":
			return p.buildLib(ctx, req.Msg.GetRef(), req.Msg.States)
		}
	}

}

func (p Provider) List(ctx context.Context, req *connect.Request[pluginv1.ListRequest], stream *connect.ServerStream[pluginv1.ListResponse]) error {
	panic("implement me")
}
