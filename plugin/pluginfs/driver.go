package pluginfs

import (
	"context"
	"os"
	"path/filepath"
	"strings"

	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/htypes"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/lib/pluginsdk"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	fsv1 "github.com/hephbuild/heph/plugin/pluginfs/gen/heph/plugin/fs/v1"
	"google.golang.org/protobuf/reflect/protodesc"
	"google.golang.org/protobuf/types/known/anypb"
)

var _ pluginsdk.Driver = (*Driver)(nil)
var _ pluginsdk.Initer = (*Driver)(nil)

type Driver struct {
	resultClient pluginsdk.InitPayload
}

const DriverName = "fs_driver"

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

	return pluginv1.ConfigResponse_builder{
		Name:         htypes.Ptr(DriverName),
		TargetSchema: pdesc,
	}.Build(), nil
}

func (p *Driver) Parse(ctx context.Context, req *pluginv1.ParseRequest) (*pluginv1.ParseResponse, error) {
	type Config struct {
		File string `mapstructure:"file"`
	}

	cfg, err := hstructpb.Decode[Config](req.GetSpec().GetConfig())
	if err != nil {
		return nil, err
	}

	var target *fsv1.Target
	if hfs.IsGlob(cfg.File) {
		target = fsv1.Target_builder{
			Pattern: htypes.Ptr(cfg.File),
		}.Build()
	} else {
		_, err := os.Stat(filepath.Join(p.resultClient.Root, cfg.File))
		if err != nil {
			return nil, err
		}

		target = fsv1.Target_builder{
			File: htypes.Ptr(cfg.File),
		}.Build()
	}

	targetAny, err := anypb.New(target)
	if err != nil {
		return nil, err
	}

	return pluginv1.ParseResponse_builder{
		Target: pluginv1.TargetDef_builder{
			Ref:                req.GetSpec().GetRef(),
			Def:                targetAny,
			Outputs:            []string{""},
			Cache:              htypes.Ptr(true),
			DisableRemoteCache: htypes.Ptr(true),
		}.Build(),
	}.Build(), nil
}

func IsExecOwner(mode os.FileMode) bool {
	return mode&0100 != 0
}

func (p *Driver) Run(ctx context.Context, req *pluginv1.RunRequest) (*pluginv1.RunResponse, error) {
	t := &fsv1.Target{}
	err := req.GetTarget().GetDef().UnmarshalTo(t)
	if err != nil {
		return nil, err
	}

	if t.HasFile() {
		path := filepath.Join(p.resultClient.Root, t.GetFile())

		info, err := os.Stat(path)
		if err != nil {
			return nil, err
		}

		return pluginv1.RunResponse_builder{
			Artifacts: []*pluginv1.Artifact{
				pluginv1.Artifact_builder{
					Name: htypes.Ptr(filepath.Base(t.GetFile())),
					Type: htypes.Ptr(pluginv1.Artifact_TYPE_OUTPUT),
					File: pluginv1.Artifact_ContentFile_builder{
						SourcePath: htypes.Ptr(path),
						OutPath:    htypes.Ptr(t.GetFile()),
						X:          htypes.Ptr(IsExecOwner(info.Mode())),
					}.Build(),
				}.Build(),
			},
		}.Build(), nil
	}

	fs := hfs.NewOS(p.resultClient.Root)

	var artifacts []*pluginv1.Artifact
	err = hfs.Glob(ctx, fs, t.GetPattern(), nil, func(path string, d hfs.DirEntry) error {
		if ctx.Err() != nil {
			return ctx.Err()
		}

		info, err := d.Info()
		if err != nil {
			return err
		}

		artifacts = append(artifacts, pluginv1.Artifact_builder{
			Name: htypes.Ptr(strings.ReplaceAll(path, "/", "_")),
			Type: htypes.Ptr(pluginv1.Artifact_TYPE_OUTPUT),
			File: pluginv1.Artifact_ContentFile_builder{
				SourcePath: htypes.Ptr(fs.At(path).Path()),
				OutPath:    htypes.Ptr(path),
				X:          htypes.Ptr(IsExecOwner(info.Mode())),
			}.Build(),
		}.Build())

		return nil
	})
	if err != nil {
		return nil, err
	}

	return pluginv1.RunResponse_builder{
		Artifacts: artifacts,
	}.Build(), nil
}

func (p *Driver) Pipe(ctx context.Context, req *pluginv1.PipeRequest) (*pluginv1.PipeResponse, error) {
	return nil, pluginsdk.ErrNotImplemented
}
