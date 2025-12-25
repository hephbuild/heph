package pluginbin

import (
	"context"
	"errors"
	"os/exec"
	"strings"

	"connectrpc.com/connect"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/lib/pluginsdk"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	binv1 "github.com/hephbuild/heph/plugin/pluginbin/gen/heph/plugin/bin/v1"
	"google.golang.org/protobuf/reflect/protodesc"
	"google.golang.org/protobuf/types/known/anypb"
)

type Plugin struct {
}

const Name = "bin"

func (p Plugin) Pipe(ctx context.Context, request *pluginv1.PipeRequest) (*pluginv1.PipeResponse, error) {
	return nil, connect.NewError(connect.CodeUnimplemented, errors.New("not implemented"))
}

func (p Plugin) Config(ctx context.Context, request *pluginv1.ConfigRequest) (*pluginv1.ConfigResponse, error) {
	desc := protodesc.ToDescriptorProto((&binv1.Target{}).ProtoReflect().Descriptor())

	return pluginv1.ConfigResponse_builder{
		Name:         htypes.Ptr(Name),
		TargetSchema: desc,
	}.Build(), nil
}

func (p Plugin) Parse(ctx context.Context, req *pluginv1.ParseRequest) (*pluginv1.ParseResponse, error) {
	binName, err := hstructpb.Decode[string](req.GetSpec().GetConfig()["name"])
	if err != nil {
		return nil, err
	}

	s := binv1.Target_builder{
		Name: &binName,
	}.Build()

	target, err := anypb.New(s)
	if err != nil {
		return nil, err
	}

	return pluginv1.ParseResponse_builder{
		Target: pluginv1.TargetDef_builder{
			Def:   target,
			Ref:   req.GetSpec().GetRef(),
			Cache: htypes.Ptr(false),
			Outputs: []*pluginv1.TargetDef_Output{pluginv1.TargetDef_Output_builder{
				Group: htypes.Ptr(""),
				Paths: []*pluginv1.TargetDef_Output_Path{pluginv1.TargetDef_Output_Path_builder{
					FilePath: htypes.Ptr(binName),
					Collect:  htypes.Ptr(false),
				}.Build()},
			}.Build()},
		}.Build(),
	}.Build(), nil
}

const wrapperScript = `
#!/usr/bin/env sh

exec <PATH> "$@" 
`

func (p Plugin) Run(ctx context.Context, req *pluginv1.RunRequest) (*pluginv1.RunResponse, error) {
	t := &binv1.Target{}
	err := req.GetTarget().GetDef().UnmarshalTo(t)
	if err != nil {
		return nil, err
	}

	path, err := exec.LookPath(t.GetName())
	if err != nil {
		return nil, err
	}

	return pluginv1.RunResponse_builder{
		Artifacts: []*pluginv1.Artifact{pluginv1.Artifact_builder{
			Name:  htypes.Ptr(t.GetName()),
			Group: htypes.Ptr(""),
			Type:  htypes.Ptr(pluginv1.Artifact_TYPE_OUTPUT),
			Raw: pluginv1.Artifact_ContentRaw_builder{
				Data: []byte(strings.TrimSpace(strings.ReplaceAll(wrapperScript, "<PATH>", path))),
				Path: htypes.Ptr(t.GetName()),
				X:    htypes.Ptr(true),
			}.Build(),
		}.Build()},
	}.Build(), nil
}

func (p Plugin) ApplyTransitive(ctx context.Context, request *pluginv1.ApplyTransitiveRequest) (*pluginv1.ApplyTransitiveResponse, error) {
	return nil, connect.NewError(connect.CodeUnimplemented, errors.New("bin doesnt support transitive"))
}

func New() *Plugin {
	return &Plugin{}
}

var _ pluginsdk.Driver = (*Plugin)(nil)
