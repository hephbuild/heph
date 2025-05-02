package pluginfs

import (
	"connectrpc.com/connect"
	"context"
	"errors"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/lib/engine"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
	fsv1 "github.com/hephbuild/heph/plugin/pluginfs/gen/heph/plugin/fs/v1"
	"google.golang.org/protobuf/reflect/protodesc"
	"google.golang.org/protobuf/types/known/anypb"
	"google.golang.org/protobuf/types/known/timestamppb"
	"os"
	"path/filepath"
)

var _ pluginv1connect.DriverHandler = (*Driver)(nil)
var _ engine.PluginIniter = (*Driver)(nil)

type Driver struct {
	resultClient engine.PluginInit
}

func NewDriver() *Driver {
	return &Driver{}
}

func (p *Driver) PluginInit(ctx context.Context, init engine.PluginInit) error {
	p.resultClient = init

	return nil
}

func (p *Driver) Config(ctx context.Context, req *connect.Request[pluginv1.ConfigRequest]) (*connect.Response[pluginv1.ConfigResponse], error) {
	desc := (&fsv1.Target{}).ProtoReflect().Descriptor()
	pdesc := protodesc.ToDescriptorProto(desc)

	return connect.NewResponse(&pluginv1.ConfigResponse{
		Name:         "fs_driver",
		TargetSchema: pdesc,
	}), nil
}

func (p *Driver) Parse(ctx context.Context, req *connect.Request[pluginv1.ParseRequest]) (*connect.Response[pluginv1.ParseResponse], error) {
	type Config struct {
		File string `mapstructure:"file"`
	}

	cfg, err := hstructpb.Decode[Config](req.Msg.Spec.Config)
	if err != nil {
		return nil, err
	}

	info, err := os.Stat(filepath.Join(p.resultClient.Root, cfg.File))
	if err != nil {
		return nil, err
	}

	target := &fsv1.Target{
		File:       cfg.File,
		ModifiedAt: timestamppb.New(info.ModTime()),
	}

	targetAny, err := anypb.New(target)
	if err != nil {
		return nil, err
	}

	return connect.NewResponse(&pluginv1.ParseResponse{
		Target: &pluginv1.TargetDef{
			Ref:     req.Msg.GetSpec().GetRef(),
			Def:     targetAny,
			Outputs: []string{""},
			Cache:   true,
		},
	}), nil
}

func (p *Driver) Run(ctx context.Context, req *connect.Request[pluginv1.RunRequest]) (*connect.Response[pluginv1.RunResponse], error) {
	t := &fsv1.Target{}
	err := req.Msg.GetTarget().GetDef().UnmarshalTo(t)
	if err != nil {
		return nil, err
	}

	return connect.NewResponse(&pluginv1.RunResponse{
		Artifacts: []*pluginv1.Artifact{
			{
				Name:     filepath.Base(t.File),
				Type:     pluginv1.Artifact_TYPE_OUTPUT,
				Encoding: pluginv1.Artifact_ENCODING_NONE,
				Uri:      "file://" + filepath.Join(p.resultClient.Root, t.File),
				Package:  filepath.Dir(t.File),
			},
		},
	}), nil
}

func (p *Driver) Pipe(ctx context.Context, req *connect.Request[pluginv1.PipeRequest]) (*connect.Response[pluginv1.PipeResponse], error) {
	return nil, connect.NewError(connect.CodeUnimplemented, errors.New("not implemented"))
}
