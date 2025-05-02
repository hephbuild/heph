package pluginexec

import (
	"fmt"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
)

type SpecStrings []string

func (s *SpecStrings) MapstructureDecode(v any) error {
	if v, err := hstructpb.Decode[string](v); err == nil {
		*s = []string{v}
		return nil
	}

	if v, err := hstructpb.DecodeSlice[string](v); err == nil {
		*s = v
		return nil
	}

	return fmt.Errorf("expected string or []string, got %T", v)
}

type SpecDeps map[string]SpecStrings //nolint:recvcheck

func (s *SpecDeps) MapstructureDecode(v any) error {
	if v, err := hstructpb.Decode[SpecStrings](v); err == nil {
		*s = map[string]SpecStrings{"": v}
		return nil
	}

	if v, err := hstructpb.Decode[map[string]SpecStrings](v); err == nil {
		*s = v
		return nil
	}

	return fmt.Errorf("expected string, []string, map[string]string or map[string][]string, got %T", v)
}

func (s SpecDeps) Merge(ds ...SpecDeps) SpecDeps {
	nd := SpecDeps{}
	for _, deps := range append([]SpecDeps{s}, ds...) {
		for k, v := range deps {
			nd[k] = append(nd[k], v...)
		}
	}

	return nd
}

type SpecOutputs map[string]SpecStrings

func (s *SpecOutputs) MapstructureDecode(v any) error {
	if v, err := hstructpb.Decode[SpecStrings](v); err == nil {
		*s = map[string]SpecStrings{"": v}
		return nil
	}

	if v, err := hstructpb.Decode[map[string]SpecStrings](v); err == nil {
		*s = v
		return nil
	}

	return fmt.Errorf("expected string, []string, map[string]string or map[string][]string, got %T", v)
}

type Spec struct {
	Run            SpecStrings       `mapstructure:"run"`
	Deps           SpecDeps          `mapstructure:"deps"`
	Tools          SpecDeps          `mapstructure:"tools"`
	HashDeps       SpecDeps          `mapstructure:"hash_deps"`
	RuntimeDeps    SpecDeps          `mapstructure:"runtime_deps"`
	Out            SpecOutputs       `mapstructure:"out"`
	Cache          bool              `mapstructure:"cache"`
	Pty            bool              `mapstructure:"pty"`
	Codegen        string            `mapstructure:"codegen"`
	Env            map[string]string `mapstructure:"env"`
	RuntimeEnv     map[string]string `mapstructure:"runtime_env"`
	PassEnv        []string          `mapstructure:"pass_env"`
	RuntimePassEnv []string          `mapstructure:"runtime_pass_env"`
	InTree         bool              `mapstructure:"in_tree"`
}
