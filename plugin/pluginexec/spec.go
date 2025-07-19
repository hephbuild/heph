package pluginexec

import (
	"errors"
	"fmt"
	"reflect"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"
)

type SpecStrings []string

func (s *SpecStrings) MapstructureDecode(v any) error {
	if v == nil {
		*s = nil

		return nil
	}

	rv := reflect.ValueOf(v)

	switch rv.Kind() { //nolint:exhaustive
	case reflect.String:
		*s = []string{rv.Interface().(string)} //nolint:errcheck
		return nil
	case reflect.Slice, reflect.Array:
		values := make([]string, 0, rv.Len())
		for i := range rv.Len() {
			elem := rv.Index(i)

			if v, ok := elem.Interface().(string); ok {
				values = append(values, v)
			} else {
				return fmt.Errorf("expected string, got %v", elem.Type())
			}
		}
		*s = values
		return nil
	default:
		return fmt.Errorf("expected string or []string, got %T", v)
	}
}

type SpecDeps map[string]SpecStrings //nolint:recvcheck

func (s *SpecDeps) MapstructureDecode(v any) error {
	if v == nil {
		*s = nil

		return nil
	}

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

type SpecCache struct {
	Local  bool
	Remote bool
}

func (s *SpecCache) MapstructureDecode(v any) error {
	if v, err := hstructpb.Decode[bool](v); err == nil {
		*s = SpecCache{
			Local:  v,
			Remote: v,
		}
		return nil
	}

	if v, err := hstructpb.Decode[string](v); err == nil {
		if v == "local" {
			*s = SpecCache{
				Local:  true,
				Remote: false,
			}
			return nil
		} else {
			return fmt.Errorf("invalid value: %q", v)
		}
	}

	return errors.New(`invalid value: must be bool or "local"`)
}

type Spec struct {
	Run            SpecStrings       `mapstructure:"run"`
	Deps           SpecDeps          `mapstructure:"deps"`
	Tools          SpecDeps          `mapstructure:"tools"`
	HashDeps       SpecDeps          `mapstructure:"hash_deps"`
	RuntimeDeps    SpecDeps          `mapstructure:"runtime_deps"`
	Out            SpecOutputs       `mapstructure:"out"`
	Cache          SpecCache         `mapstructure:"cache"`
	Pty            bool              `mapstructure:"pty"`
	Codegen        string            `mapstructure:"codegen"`
	Env            map[string]string `mapstructure:"env"`
	RuntimeEnv     map[string]string `mapstructure:"runtime_env"`
	PassEnv        []string          `mapstructure:"pass_env"`
	RuntimePassEnv []string          `mapstructure:"runtime_pass_env"`
	InTree         bool              `mapstructure:"in_tree"`
}
