package config

import (
	log "heph/hlog"
	"io"
	"os"
	"path/filepath"
	"sort"
)
import (
	"gopkg.in/yaml.v3"
)

type BaseConfig struct {
	Version  Version
	Location string
}

const (
	CacheOrderLatency = "latency"
	CacheOrderNone    = "none"
)

type Config struct {
	BaseConfig        `yaml:",inline"`
	CacheOrder        string `yaml:"cache_order"`
	Cache             map[string]Cache
	CacheHistory      int                 `yaml:"cache_history"`
	DisableGC         bool                `yaml:"disable_gc"`
	DisableCacheHints bool                `yaml:"disable_cache_hints"`
	InstallTools      bool                `yaml:"install_tools"`
	Platforms         map[string]Platform `yaml:"platforms"`
	BuildFiles        struct {
		Ignore []string        `yaml:"ignore"`
		Roots  map[string]Root `yaml:"roots"`
	} `yaml:"build_files"`
	Glob struct {
		Exclude []string `yaml:"exclude"`
	} `yaml:"glob"`
	Watch struct {
		Ignore []string `yaml:"ignore"`
	} `yaml:"watch"`
	KeepSandbox     bool              `yaml:"keep_sandbox"`
	TargetScheduler string            `yaml:"target_scheduler"`
	Params          map[string]string `yaml:"params"`
	Extras          `yaml:",inline"`

	Sources []FileConfig `yaml:"-"`
}

func (c Config) OrderedPlatforms() []Platform {
	platforms := make([]Platform, 0, len(c.Platforms))
	for _, p := range c.Platforms {
		platforms = append(platforms, p)
	}
	sort.Slice(platforms, func(i, j int) bool {
		// Order priority DESC
		return platforms[i].Priority > platforms[j].Priority
	})

	return platforms
}

type Cache struct {
	URI       string
	Read      bool
	Write     bool
	Secondary bool
}

type Root struct {
	URI string
}

type Platform struct {
	Name     string                 `yaml:"name"`
	Provider string                 `yaml:"provider"`
	Priority int                    `yaml:"priority"`
	Options  map[string]interface{} `yaml:"options"`
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
