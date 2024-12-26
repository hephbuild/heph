package pluginexec

import "fmt"

type SpecStrings []string

func (s *SpecStrings) MapstructureDecode(v any) error {
	if v, err := Decode[string](v); err == nil {
		*s = []string{v}
		return nil
	}

	if v, err := DecodeSlice[string](v); err == nil {
		*s = v
		return nil
	}

	return fmt.Errorf("expected string or []string, got %T", v)
}

type SpecDeps map[string]SpecStrings

func (s *SpecDeps) MapstructureDecode(v any) error {
	if v, err := Decode[SpecStrings](v); err == nil {
		*s = map[string]SpecStrings{"": v}
		return nil
	}

	if v, err := Decode[map[string]SpecStrings](v); err == nil {
		*s = v
		return nil
	}

	return fmt.Errorf("expected string, []string, map[string]string or map[string][]string, got %T", v)
}

type SpecOutputs map[string]SpecStrings

func (s *SpecOutputs) MapstructureDecode(v any) error {
	if v, err := Decode[SpecStrings](v); err == nil {
		*s = map[string]SpecStrings{"": v}
		return nil
	}

	if v, err := Decode[map[string]SpecStrings](v); err == nil {
		*s = v
		return nil
	}

	return fmt.Errorf("expected string, []string, map[string]string or map[string][]string, got %T", v)
}

type Spec struct {
	Run  SpecStrings `mapstructure:"run"`
	Deps SpecDeps    `mapstructure:"deps"`
	Out  SpecOutputs `mapstructure:"out"`
}
