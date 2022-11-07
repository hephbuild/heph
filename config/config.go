package config

import (
	log "github.com/sirupsen/logrus"
	"io"
	"os"
	"path/filepath"
)
import (
	"gopkg.in/yaml.v3"
)

type BaseConfig struct {
	Version  Version
	Location string
}

type Config struct {
	BaseConfig   `yaml:",inline"`
	Cache        map[string]Cache
	CacheHistory int
	DisableGC    bool `yaml:"disable_gc"`
	BuildFiles   struct {
		Ignore []string        `yaml:"ignore"`
		Roots  map[string]Root `yaml:"roots"`
	} `yaml:"build_files"`
	KeepSandbox bool              `yaml:"keep_sandbox"`
	Params      map[string]string `yaml:"params"`
	Extras      `yaml:",inline"`

	Sources []FileConfig `yaml:"-"`
}

type Cache struct {
	URI   string
	Read  bool
	Write bool
}

type Root struct {
	URI string
}

func Parse(name string) (FileConfig, error) {
	f, err := os.Open(name)
	if err != nil {
		return FileConfig{}, err
	}
	defer f.Close()

	b, err := io.ReadAll(f)
	if err != nil {
		return FileConfig{}, err
	}

	var cfg FileConfig
	err = yaml.Unmarshal(b, &cfg)
	if err != nil {
		return FileConfig{}, err
	}

	return cfg, err
}

func ParseAndApply(name string, cfg *Config) error {
	fcfg, err := Parse(name)
	if err != nil {
		return err
	}

	log.Tracef("config %v %#v", filepath.Base(name), cfg)

	*cfg = fcfg.ApplyTo(*cfg)

	return nil
}
