package engine

import (
	"fmt"
	"os"
	"slices"

	"github.com/goccy/go-yaml"
)

type YAMLConfig struct {
	Version    *string              `yaml:"version"`
	Providers  []YAMLConfigProvider `yaml:"providers"`
	Drivers    []YAMLConfigDriver   `yaml:"drivers"`
	Caches     []YAMLConfigCache    `yaml:"caches"`
	HomeDir    *string              `yaml:"homeDir"`
	Packages   YAMLConfigPackages   `yaml:"packages"`
	LockDriver *string              `yaml:"lockDriver"`
}

type YAMLConfigPackages struct {
	Exclude []string `yaml:"exclude"`
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

type YAMLConfigCache struct {
	Name    string         `yaml:"name"`
	Driver  string         `yaml:"driver"`
	Read    *bool          `yaml:"read"`
	Write   *bool          `yaml:"write"`
	Options map[string]any `yaml:"options"`
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

	if inc.HomeDir != nil {
		cfg.HomeDir = *inc.HomeDir
	}

	if inc.LockDriver != nil {
		switch *inc.LockDriver {
		case "fs":
			cfg.LockDriver = "fs"
		case "mem":
			cfg.LockDriver = "mem"
		default:
			return Config{}, fmt.Errorf("unknown lock driver: %s", *inc.LockDriver)
		}
	}

	cfg.Packages.Exclude = append(cfg.Packages.Exclude, inc.Packages.Exclude...)

	for _, incc := range inc.Caches {
		i := slices.IndexFunc(cfg.Caches, func(p ConfigCache) bool {
			return p.Name == incc.Name
		})

		if i < 0 {
			cfg.Caches = append(cfg.Caches, ConfigCache{
				Name:   incc.Name,
				Driver: incc.Driver,
			})
			i = len(cfg.Caches) - 1
		}

		cc := cfg.Caches[i]
		if incc.Read != nil {
			cc.Read = *incc.Read
		}
		if incc.Write != nil {
			cc.Write = *incc.Write
		}
		if incc.Options != nil {
			if cc.Options == nil {
				cc.Options = make(map[string]any)
			}
			for k, v := range incc.Options {
				cc.Options[k] = v
			}
		}
		cfg.Caches[i] = cc
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
