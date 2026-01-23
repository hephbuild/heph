package pluginfs

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/internal/htypes"
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
	cfg, err := hstructpb.Decode[struct {
		File string `mapstructure:"file"`

		Glob    string   `mapstructure:"glob"`
		Exclude []string `mapstructure:"exclude"`
	}](req.GetSpec().GetConfig())
	if err != nil {
		return nil, err
	}

	var target *fsv1.Target
	var outPaths []*pluginv1.TargetDef_Output_Path
	if cfg.Glob != "" {
		target = fsv1.Target_builder{
			Pattern: htypes.Ptr(cfg.Glob),
			Exclude: cfg.Exclude,
		}.Build()
		outPaths = []*pluginv1.TargetDef_Output_Path{pluginv1.TargetDef_Output_Path_builder{
			Glob:    htypes.Ptr(cfg.Glob),
			Collect: htypes.Ptr(false),
		}.Build()}
	} else {
		_, err := os.Stat(filepath.Join(p.resultClient.Root, cfg.File))
		if err != nil {
			return nil, err
		}

		target = fsv1.Target_builder{
			File: htypes.Ptr(cfg.File),
		}.Build()
		outPaths = []*pluginv1.TargetDef_Output_Path{pluginv1.TargetDef_Output_Path_builder{
			FilePath: htypes.Ptr(cfg.File),
			Collect:  htypes.Ptr(false),
		}.Build()}
	}

	targetAny, err := anypb.New(target)
	if err != nil {
		return nil, err
	}

	return pluginv1.ParseResponse_builder{
		Target: pluginv1.TargetDef_builder{
			Ref: req.GetSpec().GetRef(),
			Def: targetAny,
			Outputs: []*pluginv1.TargetDef_Output{pluginv1.TargetDef_Output_builder{
				Group: htypes.Ptr(""),
				Paths: outPaths,
			}.Build()},
			Cache:              htypes.Ptr(false),
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

		if IsCodegen(ctx, path) {
			return pluginv1.RunResponse_builder{
				Artifacts: []*pluginv1.Artifact{},
			}.Build(), nil
		}

		excluded, err := hfs.PathMatchAny(path, t.GetExclude()...)
		if err != nil {
			return nil, err
		}

		if excluded {
			return pluginv1.RunResponse_builder{
				Artifacts: []*pluginv1.Artifact{},
			}.Build(), nil
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
		info, err := d.Info()
		if err != nil {
			return err
		}

		if IsCodegen(ctx, fs.At(path).Path()) {
			return nil
		}

		excluded, err := hfs.PathMatchAny(fs.At(path).Path(), t.GetExclude()...)
		if err != nil {
			return err
		}

		if excluded {
			return nil
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

func (p *Driver) ApplyTransitive(ctx context.Context, request *pluginv1.ApplyTransitiveRequest) (*pluginv1.ApplyTransitiveResponse, error) {
	return nil, fmt.Errorf("%w: files doesnt support transitive", pluginsdk.ErrNotImplemented)
}

func (p *Driver) Pipe(ctx context.Context, req *pluginv1.PipeRequest) (*pluginv1.PipeResponse, error) {
	return nil, pluginsdk.ErrNotImplemented
}
