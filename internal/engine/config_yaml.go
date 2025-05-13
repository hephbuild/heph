package engine

import (
	"github.com/goccy/go-yaml"
	"os"
	"slices"
)

type YAMLConfig struct {
	Version   *string              `yaml:"version"`
	Providers []YAMLConfigProvider `yaml:"providers"`
	Drivers   []YAMLConfigDriver   `yaml:"drivers"`
}

type YAMLConfigProvider struct {
	YAMLConfigPlugin `yaml:",inline"`
}

type YAMLConfigDriver struct {
	YAMLConfigPlugin `yaml:",inline"`
}

type YAMLConfigPlugin struct {
	Name    string         `yaml:"name"`
	Enabled *bool          `yaml:"enabled"`
	Options map[string]any `yaml:"options,omitempty"`
}

func ParseYAMLConfig(filepath string) (YAMLConfig, error) {
	b, err := os.ReadFile(filepath)
	if err != nil {
		return YAMLConfig{}, err
	}

	var cfg YAMLConfig
	err = yaml.UnmarshalWithOptions(b, &cfg, yaml.Strict())
	if err != nil {
		return YAMLConfig{}, err
	}

	return cfg, nil
}

func ApplyYAMLConfig(cfg Config, inc YAMLConfig) (Config, error) {
	if inc.Version != nil {
		cfg.Version = *inc.Version
	}

	for _, incp := range inc.Providers {
		i := slices.IndexFunc(cfg.Providers, func(p ConfigProvider) bool {
			return p.Name == incp.Name
		})

		if i < 0 {
			cfg.Providers = append(cfg.Providers, ConfigProvider{
				Name: incp.Name,
			})
			i = len(cfg.Providers) - 1
		}

		cp := cfg.Providers[i]
		if incp.Enabled != nil {
			cp.Enabled = *incp.Enabled
		}
		if incp.Options != nil {
			cp.Options = incp.Options
		}
		cfg.Providers[i] = cp
	}

	for _, incd := range inc.Drivers {
		i := slices.IndexFunc(cfg.Drivers, func(p ConfigDriver) bool {
			return p.Name == incd.Name
		})

		if i < 0 {
			cfg.Drivers = append(cfg.Drivers, ConfigDriver{
				Name: incd.Name,
			})
			i = len(cfg.Drivers) - 1
		}

		cd := cfg.Drivers[i]
		if incd.Enabled != nil {
			cd.Enabled = *incd.Enabled
		}
		if incd.Options != nil {
			cd.Options = incd.Options
		}
		cfg.Drivers[i] = cd
	}

	return cfg, nil
}
