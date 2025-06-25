package pluginsmartprovidertest

import (
	"context"
	"fmt"
	"io"

	"github.com/hephbuild/heph/internal/engine"

	"github.com/hephbuild/heph/internal/hartifact"
	corev1 "github.com/hephbuild/heph/plugin/gen/heph/core/v1"
	"google.golang.org/protobuf/types/known/structpb"

	engine2 "github.com/hephbuild/heph/lib/engine"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

var _ engine2.Provider = (*Provider)(nil)
var _ engine2.PluginIniter = (*Provider)(nil)

const ProviderName = "smart_provider_test"

func New() *Provider {
	return &Provider{}
}

type Provider struct {
	resultClient engine2.EngineHandle
}

func (p *Provider) PluginInit(ctx context.Context, init engine.PluginInit) error {
	p.resultClient = init.CoreHandle

	return nil
}

func (p *Provider) Config(ctx context.Context, req *pluginv1.ProviderConfigRequest) (*pluginv1.ProviderConfigResponse, error) {
	return &pluginv1.ProviderConfigResponse{
		Name: ProviderName,
	}, nil
}

func (p *Provider) List(ctx context.Context, req *pluginv1.ListRequest) (engine2.HandlerStreamReceive[*pluginv1.ListResponse], error) {
	return engine2.NewNoopChanHandlerStream[*pluginv1.ListResponse](), nil
}

func (p *Provider) Get(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.GetResponse, error) {
	res, err := p.resultClient.ResultClient.Get(ctx, &corev1.ResultRequest{
		RequestId: req.RequestId,
		Of: &corev1.ResultRequest_Spec{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "some/package",
					Name:    "think",
				},
				Driver: "bash",
				Config: map[string]*structpb.Value{
					"out": structpb.NewStringValue("out"),
					"run": structpb.NewStringValue(`echo hello > $OUT`),
				},
			},
		},
	})
	if err != nil {
		return nil, err
	}

	artifacts := hartifact.FindOutputs(res.GetArtifacts(), "")

	r, err := hartifact.FileReader(ctx, artifacts[0])
	if err != nil {
		return nil, err
	}
	defer r.Close()

	b, err := io.ReadAll(r)
	if err != nil {
		return nil, err
	}

	return &pluginv1.GetResponse{
		Spec: &pluginv1.TargetSpec{
			Ref:    req.GetRef(),
			Driver: "bash",
			Config: map[string]*structpb.Value{
				"out": structpb.NewStringValue("out"),
				"run": structpb.NewStringValue(fmt.Sprintf(`echo 'parent: %s' > $OUT`, b)),
			},
		},
	}, nil
}

func (p *Provider) Probe(ctx context.Context, req *pluginv1.ProbeRequest) (*pluginv1.ProbeResponse, error) {
	return &pluginv1.ProbeResponse{}, nil
}
