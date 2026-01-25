package plugintextfile

import (
	"context"
	"fmt"
	"path/filepath"

	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/lib/tref"
	textfilev1 "github.com/hephbuild/heph/plugin/plugintextfile/gen/heph/plugin/textfile/v1"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/lib/pluginsdk"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	groupv1 "github.com/hephbuild/heph/plugin/plugingroup/gen/heph/plugin/group/v1"
	"google.golang.org/protobuf/reflect/protodesc"
	"google.golang.org/protobuf/types/known/anypb"
)

type Plugin struct {
}

const Name = "textfile"

func (p Plugin) Pipe(ctx context.Context, request *pluginv1.PipeRequest) (*pluginv1.PipeResponse, error) {
	return nil, fmt.Errorf("%w, textfile doesnt support pipe", pluginsdk.ErrNotImplemented)
}

func (p Plugin) Config(ctx context.Context, request *pluginv1.ConfigRequest) (*pluginv1.ConfigResponse, error) {
	desc := protodesc.ToDescriptorProto((&groupv1.Target{}).ProtoReflect().Descriptor())

	return pluginv1.ConfigResponse_builder{
		Name:         htypes.Ptr(Name),
		TargetSchema: desc,
	}.Build(), nil
}

func (p Plugin) Parse(ctx context.Context, req *pluginv1.ParseRequest) (*pluginv1.ParseResponse, error) {
	text, err := hstructpb.Decode[string](req.GetSpec().GetConfig()["text"])
	if err != nil {
		return nil, err
	}

	output := req.GetSpec().GetRef().GetName()
	if v, ok := req.GetSpec().GetConfig()["out"]; ok {
		output, err = hstructpb.Decode[string](v)
		if err != nil {
			return nil, err
		}
	}

	s := textfilev1.Target_builder{
		Text:   htypes.Ptr(text),
		Output: htypes.Ptr(output),
	}.Build()

	target, err := anypb.New(s)
	if err != nil {
		return nil, err
	}

	return pluginv1.ParseResponse_builder{
		Target: pluginv1.TargetDef_builder{
			Def:                target,
			Ref:                req.GetSpec().GetRef(),
			Cache:              htypes.Ptr(true),
			DisableRemoteCache: htypes.Ptr(true),
			Outputs: []*pluginv1.TargetDef_Output{pluginv1.TargetDef_Output_builder{
				Group: htypes.Ptr(""),
				Paths: []*pluginv1.TargetDef_Path{pluginv1.TargetDef_Path_builder{
					FilePath: htypes.Ptr(s.GetOutput()),
				}.Build()},
			}.Build()},
		}.Build(),
	}.Build(), nil
}

func (p Plugin) Run(ctx context.Context, req *pluginv1.RunRequest) (*pluginv1.RunResponse, error) {
	t := &textfilev1.Target{}
	err := req.GetTarget().GetDef().UnmarshalTo(t)
	if err != nil {
		return nil, err
	}

	return pluginv1.RunResponse_builder{
		Artifacts: []*pluginv1.Artifact{
			pluginv1.Artifact_builder{
				Group: htypes.Ptr(""),
				Name:  htypes.Ptr(t.GetOutput()),
				Type:  htypes.Ptr(pluginv1.Artifact_TYPE_OUTPUT),
				Raw: pluginv1.Artifact_ContentRaw_builder{
					Data: []byte(t.GetText()),
					Path: htypes.Ptr(filepath.Join(tref.ToOSPath(req.GetTarget().GetRef().GetPackage()), t.GetOutput())),
				}.Build(),
			}.Build(),
		},
	}.Build(), nil
}

func (p Plugin) ApplyTransitive(ctx context.Context, request *pluginv1.ApplyTransitiveRequest) (*pluginv1.ApplyTransitiveResponse, error) {
	return nil, fmt.Errorf("%w, textfile doesnt support transitive", pluginsdk.ErrNotImplemented)
}

func New() *Plugin {
	return &Plugin{}
}

var _ pluginsdk.Driver = (*Plugin)(nil)
