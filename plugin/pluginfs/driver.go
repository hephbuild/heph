package pluginfs

import (
	"context"
	"os"
	"path/filepath"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/lib/pluginsdk"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	fsv1 "github.com/hephbuild/heph/plugin/pluginfs/gen/heph/plugin/fs/v1"
	"google.golang.org/protobuf/reflect/protodesc"
	"google.golang.org/protobuf/types/known/anypb"
	"google.golang.org/protobuf/types/known/timestamppb"
)

var _ pluginsdk.Driver = (*Driver)(nil)
var _ pluginsdk.Initer = (*Driver)(nil)

type Driver struct {
	resultClient pluginsdk.InitPayload
}

func NewDriver() *Driver {
	return &Driver{}
}

func (p *Driver) PluginInit(ctx context.Context, init pluginsdk.InitPayload) error {
	p.resultClient = init

	return nil
}

func (p *Driver) Config(ctx context.Context, req *pluginv1.ConfigRequest) (*pluginv1.ConfigResponse, error) {
	desc := (&fsv1.Target{}).ProtoReflect().Descriptor()
	pdesc := protodesc.ToDescriptorProto(desc)

	return &pluginv1.ConfigResponse{
		Name:         "fs_driver",
		TargetSchema: pdesc,
	}, nil
}

func (p *Driver) Parse(ctx context.Context, req *pluginv1.ParseRequest) (*pluginv1.ParseResponse, error) {
	type Config struct {
		File string `mapstructure:"file"`
	}

	cfg, err := hstructpb.Decode[Config](req.GetSpec().GetConfig())
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

	return &pluginv1.ParseResponse{
		Target: &pluginv1.TargetDef{
			Ref:                req.GetSpec().GetRef(),
			Def:                targetAny,
			Outputs:            []string{""},
			Cache:              true,
			DisableRemoteCache: true,
		},
	}, nil
}

func (p *Driver) Run(ctx context.Context, req *pluginv1.RunRequest) (*pluginv1.RunResponse, error) {
	t := &fsv1.Target{}
	err := req.GetTarget().GetDef().UnmarshalTo(t)
	if err != nil {
		return nil, err
	}

	return &pluginv1.RunResponse{
		Artifacts: []*pluginv1.Artifact{
			{
				Name: filepath.Base(t.GetFile()),
				Type: pluginv1.Artifact_TYPE_OUTPUT,
				Content: &pluginv1.Artifact_File{File: &pluginv1.Artifact_ContentFile{
					SourcePath: filepath.Join(p.resultClient.Root, t.GetFile()),
					OutPath:    t.GetFile(),
				}},
			},
		},
	}, nil
}

func (p *Driver) Pipe(ctx context.Context, req *pluginv1.PipeRequest) (*pluginv1.PipeResponse, error) {
	return nil, pluginsdk.ErrNotImplemented
}
