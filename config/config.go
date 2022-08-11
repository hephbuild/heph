package config

import (
	"github.com/coreos/go-semver/semver"
	"io"
	"os"
	"strings"
)
import (
	"gopkg.in/yaml.v3"
)

type Config struct {
	Version    Version
	Location   string
	Cache      map[string]Cache `yaml:",omitempty"`
	BuildFiles struct {
		Ignore []string `yaml:",omitempty"`
	} `yaml:"build_files"`
}

type Version struct {
	String string
	Semver *semver.Version
	GTE    bool
}

func (e *Version) UnmarshalYAML(unmarshal func(interface{}) error) error {
	err := unmarshal(&e.String)
	if err != nil {
		return err
	}

	e.String = strings.TrimSpace(e.String)

	version := e.String
	if strings.HasPrefix(version, ">=") {
		e.GTE = true
		version = strings.TrimPrefix(version, ">=")
		version = strings.TrimSpace(version)
	}

	e.Semver, err = semver.NewVersion(version)
	if err != nil {
		if e.GTE {
			// If its gte, it has to be a semver, report errors if not
			return err
		}
	}

	return nil
}

type Cache struct {
	URI   string
	Read  *bool `yaml:",omitempty"`
	Write *bool `yaml:",omitempty"`
}

func (c Cache) Merge(nc Cache) Cache {
	if nc.URI != "" {
		c.URI = nc.URI
	}

	if nc.Read != nil {
		c.Read = nc.Read
	}

	if nc.Write != nil {
		c.Write = nc.Write
	}

	return c
}

func Parse(name string) (Config, error) {
	f, err := os.Open(name)
	if err != nil {
		return Config{}, err
	}
	defer f.Close()

	b, err := io.ReadAll(f)
	if err != nil {
		return Config{}, err
	}

	var cfg Config
	err = yaml.Unmarshal(b, &cfg)
	if err != nil {
		return Config{}, err
	}

	return cfg, err
}

func (cc Config) Merge(nc Config) Config {
	if nc.Version.String != "" {
		cc.Version = nc.Version
	}

	if nc.Location != "" {
		cc.Location = nc.Location
	}

	if cc.Cache == nil {
		cc.Cache = map[string]Cache{}
	}

	if len(nc.Cache) == 0 && nc.Cache != nil {
		cc.Cache = nil
	} else {
		for k, newCache := range nc.Cache {
			cc.Cache[k] = cc.Cache[k].Merge(newCache)
		}
	}

	cc.BuildFiles.Ignore = append(cc.BuildFiles.Ignore, nc.BuildFiles.Ignore...)

	return cc
}
