package plugingroup

import (
	"context"
	"errors"
	"strconv"

	"github.com/hephbuild/heph/lib/tref"

	"connectrpc.com/connect"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/lib/pluginsdk"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	groupv1 "github.com/hephbuild/heph/plugin/plugingroup/gen/heph/plugin/group/v1"
	"google.golang.org/protobuf/reflect/protodesc"
	"google.golang.org/protobuf/types/known/anypb"
)

type Plugin struct {
}

const Name = "group"

func (p Plugin) Pipe(ctx context.Context, request *pluginv1.PipeRequest) (*pluginv1.PipeResponse, error) {
	return nil, connect.NewError(connect.CodeUnimplemented, errors.New("not implemented"))
}

func (p Plugin) Config(ctx context.Context, request *pluginv1.ConfigRequest) (*pluginv1.ConfigResponse, error) {
	desc := protodesc.ToDescriptorProto((&groupv1.Target{}).ProtoReflect().Descriptor())

	return &pluginv1.ConfigResponse{
		Name:         Name,
		TargetSchema: desc,
	}, nil
}

func (p Plugin) Parse(ctx context.Context, req *pluginv1.ParseRequest) (*pluginv1.ParseResponse, error) {
	deps, err := hstructpb.DecodeSlice[string](req.GetSpec().GetConfig()["deps"])
	if err != nil {
		return nil, err
	}

	inputs := make([]*pluginv1.TargetDef_Input, 0, len(deps))
	for i, dep := range deps {
		ref, err := tref.ParseWithOut(dep)
		if err != nil {
			return nil, err
		}

		inputs = append(inputs, &pluginv1.TargetDef_Input{
			Ref: ref,
			Origin: &pluginv1.TargetDef_InputOrigin{
				Id: strconv.Itoa(i),
			},
		})
	}

	s := &groupv1.Target{}

	target, err := anypb.New(s)
	if err != nil {
		return nil, err
	}

	return &pluginv1.ParseResponse{
		Target: &pluginv1.TargetDef{
			Def:     target,
			Ref:     req.GetSpec().GetRef(),
			Cache:   false,
			Inputs:  inputs,
			Outputs: []string{""},
		},
	}, nil
}

func (p Plugin) Run(ctx context.Context, request *pluginv1.RunRequest) (*pluginv1.RunResponse, error) {
	panic("should not be called")
}

func New() *Plugin {
	return &Plugin{}
}

var _ pluginsdk.Driver = (*Plugin)(nil)
