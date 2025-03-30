//go:build ignore

package plugingo

import (
	"connectrpc.com/connect"
	"context"
	"fmt"
	"github.com/hephbuild/heph/internal/hfs"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
	"github.com/hephbuild/heph/plugin/tref"
)

var _ pluginv1connect.DriverHandler = (*Driver)(nil)

const DriverName = "go_build"

func NewDriver(fs hfs.FS) *Driver {
	return &Driver{
		repoRoot: fs,
	}
}

type Driver struct {
	repoRoot hfs.FS
}

func (d Driver) Config(ctx context.Context, req *connect.Request[pluginv1.ConfigRequest]) (*connect.Response[pluginv1.ConfigResponse], error) {
	return connect.NewResponse(&pluginv1.ConfigResponse{
		Name: DriverName,
	}), nil
}

func (d Driver) Parse(ctx context.Context, req *connect.Request[pluginv1.ParseRequest]) (*connect.Response[pluginv1.ParseResponse], error) {
	// TODO implement me
	panic("implement me")
}

func (d Driver) compile(ctx context.Context) error {
	/*
	   extra = ""
	   if abi:
	       extra += " -symabis $SRC_ABI -asmhdr $SRC_ABI_H"

	   if embed_cfg:
	       extra += " -embedcfg $SRC_EMBED"

	   if complete:
	       extra += " -complete"

	   extra += " -p " + import_path

	   return [
	       'echo "Compiling..."',
	       'go tool compile -importcfg $SANDBOX/importconfig -trimpath "$ROOT;$GO_OUTDIR" -o $SANDBOX/$OUT_A -pack{} $SRC_SRC'.format(extra),
	       'echo "packagefile {}=$OUT_A" > $SANDBOX/$OUT_IMPORTCFG'.format(import_path),
	   ]
	*/

	return nil
}

func (d Driver) Run(ctx context.Context, req *connect.Request[pluginv1.RunRequest]) (*connect.Response[pluginv1.RunResponse], error) {
	hpackage := req.Msg.Target.Ref.Package
	hname := req.Msg.Target.Ref.Name
	if m, err := matchThirdparty.Match(hpackage); err == nil {
		repo := m["repo"]
		version := m["version"]
		pkg := m["package"]

		switch hname {
		case "download":
			// go mod download
		case "build":
			// then build
		}
	} else if m, err := matchGo.Match(hpackage); err == nil {
		switch hname {
		case "go":
			// download go toolchain, return go bin
		case "build":
			// then build
		}
	} else if m, err := matchStd.Match(hpackage); err == nil {
		pkg := m["package"]

		switch hname {
		case "build":
			// then build
		}
	}

	return nil, fmt.Errorf("unknown target %v", tref.Format(req.Msg.Target.Ref))
}

func (d Driver) Pipe(ctx context.Context, c *connect.Request[pluginv1.PipeRequest]) (*connect.Response[pluginv1.PipeResponse], error) {
	return nil, connect.NewError(connect.CodeUnimplemented, nil)
}
