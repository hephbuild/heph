package pluginnix

import (
	"context"
	"strings"

	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/lib/pluginsdk"
	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"google.golang.org/protobuf/types/known/structpb"
)

const NameProvider = "nix"

var _ pluginsdk.Provider = (*Provider)(nil)

type Provider struct {
}

func (p *Provider) Config(ctx context.Context, c *pluginv1.ProviderConfigRequest) (*pluginv1.ProviderConfigResponse, error) {
	return pluginv1.ProviderConfigResponse_builder{
		Name: htypes.Ptr(NameProvider),
	}.Build(), nil
}

func (p *Provider) Probe(ctx context.Context, c *pluginv1.ProbeRequest) (*pluginv1.ProbeResponse, error) {
	return &pluginv1.ProbeResponse{}, nil
}

func NewProvider() *Provider {
	return &Provider{}
}

func (p *Provider) List(ctx context.Context, req *pluginv1.ListRequest) (pluginsdk.HandlerStreamReceive[*pluginv1.ListResponse], error) {
	return pluginsdk.NewNoopChanHandlerStream[*pluginv1.ListResponse](), nil
}

const wrapperScript = `
#!/usr/bin/env sh

export NIX_CONFIG=$(cat <<'EOF'
<CONFIG>
EOF
)

exec nix run nixpkgs#<PKG> --offline -- "$@" 
`

func (p *Provider) Get(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.GetResponse, error) {
	if req.GetRef().GetPackage() != "@nix" {
		return nil, pluginsdk.ErrNotFound
	}

	script := strings.NewReplacer(
		"<CONFIG>", nixConfig,
		"<PKG>", req.GetRef().GetName(),
	).Replace(wrapperScript)
	script = strings.TrimSpace(script)

	return pluginv1.GetResponse_builder{
		Spec: pluginv1.TargetSpec_builder{
			Ref:    tref.New(req.GetRef().GetPackage(), req.GetRef().GetName(), nil),
			Driver: htypes.Ptr("textfile"),
			Config: map[string]*structpb.Value{
				"text": structpb.NewStringValue(script),
				"out":  structpb.NewStringValue(req.GetRef().GetName()),
			},
			Labels: nil,
		}.Build(),
	}.Build(), nil
}
