package plugingo

import (
	"connectrpc.com/connect"
	"context"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/plugin/gen/heph/core/v1/corev1connect"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
)

type Provider struct {
	repoRoot     hfs.FS
	resultClient corev1connect.ResultServiceClient
}

var _ pluginv1connect.ProviderHandler = (*Provider)(nil)

const ProviderName = "go"

func NewProvider(fs hfs.FS) *Provider {
	return &Provider{
		repoRoot: fs,
	}
}

func (p Provider) Config(ctx context.Context, req *connect.Request[pluginv1.ProviderConfigRequest]) (*connect.Response[pluginv1.ProviderConfigResponse], error) {
	return connect.NewResponse(&pluginv1.ProviderConfigResponse{
		Name: ProviderName,
	}), nil
}

func (p Provider) Get(ctx context.Context, req *connect.Request[pluginv1.GetRequest]) (*connect.Response[pluginv1.GetResponse], error) {
	panic("implement me")
}

//func (p Provider) Get(ctx context.Context, req *connect.Request[pluginv1.GetRequest]) (*connect.Response[pluginv1.GetResponse], error) {
//	if len(req.Msg.GetStates()) == 0 {
//		return nil, connect.NewError(connect.CodeNotFound, errors.New("state not found"))
//	}
//
//	hpackage := req.Msg.Ref.Package
//	hname := req.Msg.Ref.Name
//	if m, ok := matchThirdparty.Match(hpackage); ok {
//		repo := m["repo"]
//		version := m["version"]
//		pkg := m["package"]
//
//		switch hname {
//		case "download":
//			// go mod download
//		case "build_lib":
//			return p.buildThirdpartyLib(ctx, repo, version, pkg)
//		case "build":
//			return p.buildBin(ctx, req.Msg.GetRef(), req.Msg.States)
//		}
//	} else if m, ok := matchGo.Match(hpackage); ok {
//		version := m["version"]
//
//		_ = version
//		panic("implement me: " + hpackage)
//
//		switch hname {
//		case "toolchain":
//			// download go toolchain
//		case "go":
//			// return go bin
//		}
//	} else if m, ok := matchStd.Match(hpackage); ok {
//		version := m["version"]
//		pkg := m["package"]
//
//		switch hname {
//		case "build_lib":
//			return p.buildStdLib(ctx, version, pkg)
//		}
//	} else {
//		switch hname {
//		case "build":
//			return p.buildBin(ctx, req.Msg.GetRef(), req.Msg.States)
//		case "build_test_lib":
//			return p.buildTestLib(ctx, false, req.Msg.GetRef(), req.Msg.States)
//		case "build_xtest_lib":
//			return p.buildTestLib(ctx, true, req.Msg.GetRef(), req.Msg.States)
//		case "build_test":
//			return p.buildTestBin(ctx, req.Msg.GetRef(), req.Msg.States)
//		case "test":
//			return p.runTest(ctx, req.Msg.GetRef(), req.Msg.States)
//		case "build_lib":
//			return p.buildLib(ctx, req.Msg.GetRef(), req.Msg.States)
//		}
//	}
//
//	return nil, connect.NewError(connect.CodeInvalidArgument, fmt.Errorf("unhandled target: %v", tref.Format(req.Msg.GetRef())))
//}

func (p Provider) Probe(ctx context.Context, req *connect.Request[pluginv1.ProbeRequest]) (*connect.Response[pluginv1.ProbeResponse], error) {
	return connect.NewResponse(&pluginv1.ProbeResponse{
		States: nil,
	}), nil
}
