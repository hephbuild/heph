package pluginnix

import (
	"context"
	"fmt"
	"strings"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/lib/pluginsdk"
	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"google.golang.org/protobuf/types/known/structpb"
)

const NameProvider = "nix"

var _ pluginsdk.Provider = (*Provider)(nil)

type Provider struct {
	nixToolRef *tref.Ref
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
	return &Provider{
		nixToolRef: tref.New("@heph/bin", "nix", nil),
	}
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

exec nix run <REF> -- "$@" 
`

func (p *Provider) Get(ctx context.Context, req *pluginv1.GetRequest) (*pluginv1.GetResponse, error) {
	nixRef, ok := strings.CutPrefix(req.GetRef().GetPackage(), "@nix")
	if !ok {
		return nil, pluginsdk.ErrNotFound
	}

	// https://nix.dev/manual/nix/2.24/command-ref/new-cli/nix3-flake#examples

	nixRef = strings.TrimPrefix(nixRef, "/")
	if nixRef == "" {
		nixRef = "nixpkgs"
	}

	nixPkg := req.GetRef().GetName()

	if req.GetRef().GetArgs()["install"] == "1" {
		return p.install(ctx, nixRef, nixPkg)
	} else {
		hash := true
		if v := req.GetRef().GetArgs()["hash"]; v != "" {
			hash = v == "0"
		}

		return p.get(ctx, nixRef, nixPkg, hash)
	}
}

// temporary until we get can heph heph to manage the nix cache
func (p *Provider) install(ctx context.Context, nixRef, nixPkg string) (*pluginv1.GetResponse, error) {
	ref := tref.New(tref.JoinPackage("@nix", nixRef), nixPkg, map[string]string{
		"install": "1",
	})

	return pluginv1.GetResponse_builder{
		Spec: pluginv1.TargetSpec_builder{
			Ref:    ref,
			Driver: htypes.Ptr("sh"),
			Config: map[string]*structpb.Value{
				"run": structpb.NewStringValue(fmt.Sprintf("nix hash path $(nix build %v#%v --print-out-paths) > $OUT", nixRef, nixPkg)),
				"env": hstructpb.NewMapStringStringValue(map[string]string{
					"NIX_CONFIG": nixConfig,
				}),
				"tools": structpb.NewStringValue(tref.Format(tref.WithOut(p.nixToolRef, ""))),
				"out":   structpb.NewStringValue(fmt.Sprintf("nix_hash_%v_%v", nixRef, nixPkg)),
			},
		}.Build(),
	}.Build(), nil
}

func (p *Provider) get(ctx context.Context, nixRef, nixPkg string, hash bool) (*pluginv1.GetResponse, error) {
	script := strings.NewReplacer(
		"<CONFIG>", nixConfig,
		"<REF>", fmt.Sprintf("%v#%v", nixRef, nixPkg),
	).Replace(wrapperScript)
	script = strings.TrimSpace(script)

	var args map[string]string
	if hash {
		args = map[string]string{"hash": "1"}
	} else {
		args = map[string]string{"hash": "0"}
	}

	ref := tref.New(tref.JoinPackage("@nix", nixRef), nixPkg, args)

	return pluginv1.GetResponse_builder{
		Spec: pluginv1.TargetSpec_builder{
			Ref:    ref,
			Driver: htypes.Ptr("textfile"),
			Config: map[string]*structpb.Value{
				"text": structpb.NewStringValue(script),
				"out":  structpb.NewStringValue(nixPkg),
			},
			Transitive: pluginv1.Sandbox_builder{
				Tools: []*pluginv1.Sandbox_Tool{
					pluginv1.Sandbox_Tool_builder{
						Ref:  tref.WithOut(p.nixToolRef, ""),
						Hash: htypes.Ptr(false),
					}.Build(),
				},
				Deps: []*pluginv1.Sandbox_Dep{
					pluginv1.Sandbox_Dep_builder{
						Ref:     tref.WithOut(tref.WithArgs(ref, map[string]string{"install": "1"}), ""),
						Hash:    htypes.Ptr(hash),
						Runtime: htypes.Ptr(false),
					}.Build(),
				},
			}.Build(),
		}.Build(),
	}.Build(), nil
}
