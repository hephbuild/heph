package plugingroup

import (
	"context"
	"errors"

	"connectrpc.com/connect"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
	groupv1 "github.com/hephbuild/heph/plugin/plugingroup/gen/heph/plugin/group/v1"
	"google.golang.org/protobuf/reflect/protodesc"
	"google.golang.org/protobuf/types/known/anypb"
)

type Plugin struct {
}

func (p Plugin) Pipe(ctx context.Context, c *connect.Request[pluginv1.PipeRequest]) (*connect.Response[pluginv1.PipeResponse], error) {
	return nil, connect.NewError(connect.CodeUnimplemented, errors.New("not implemented"))
}

func (p Plugin) Config(ctx context.Context, c *connect.Request[pluginv1.ConfigRequest]) (*connect.Response[pluginv1.ConfigResponse], error) {
	desc := protodesc.ToDescriptorProto((&groupv1.Target{}).ProtoReflect().Descriptor())

	return connect.NewResponse(&pluginv1.ConfigResponse{
		Name:         "group",
		TargetSchema: desc,
	}), nil
}

func (p Plugin) Parse(ctx context.Context, req *connect.Request[pluginv1.ParseRequest]) (*connect.Response[pluginv1.ParseResponse], error) {
	s := &groupv1.Target{}

	target, err := anypb.New(s)
	if err != nil {
		return nil, err
	}

	return connect.NewResponse(&pluginv1.ParseResponse{
		Target: &pluginv1.TargetDef{Def: target},
	}), nil
}

func (p Plugin) Run(ctx context.Context, req *connect.Request[pluginv1.RunRequest]) (*connect.Response[pluginv1.RunResponse], error) {
	var t groupv1.Target
	err := req.Msg.GetTarget().GetDef().UnmarshalTo(&t)
	if err != nil {
		return nil, err
	}

	artifacts := make([]*pluginv1.Artifact, 0, len(req.Msg.GetInputs()))
	for _, input := range req.Msg.GetInputs() {
		artifact := input.GetArtifact()

		artifacts = append(artifacts, &pluginv1.Artifact{
			Name:     artifact.GetName(),
			Group:    artifact.GetGroup(),
			Encoding: artifact.GetEncoding(),
			Uri:      artifact.GetUri(),
		})
	}

	return connect.NewResponse(&pluginv1.RunResponse{
		Artifacts: artifacts,
	}), nil
}

func New() *Plugin {
	return &Plugin{}
}

var _ pluginv1connect.DriverHandler = (*Plugin)(nil)
