package config

import (
	"cmp"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/mds"
	"golang.org/x/exp/slices"
	"gopkg.in/yaml.v3"
	"os"
	"path/filepath"
	"time"
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
	BaseConfig       `yaml:",inline"`
	Caches           map[string]Cache
	CacheOrder       string        `yaml:"cache_order"`
	CacheHistory     int           `yaml:"cache_history"`
	ProgressInterval time.Duration `yaml:"progress_interval"`
	Cloud            struct {
		URL     string `yaml:"url"`
		Project string `yaml:"project"`
	} `yaml:"cloud"`
	Engine struct {
		GC              bool `yaml:"gc"`
		CacheRW         bool `yaml:"cache_rw"`
		CacheHints      bool `yaml:"cache_hints"`
		GitCacheHints   bool `yaml:"git_cache_hints"`
		InstallTools    bool `yaml:"install_tools"`
		KeepSandbox     bool `yaml:"keep_sandbox"`
		ParallelCaching bool `yaml:"parallel_caching"`
		SmartGen        bool `yaml:"smart_gen"`
	} `yaml:"engine"`
	Platforms  map[string]Platform `yaml:"platforms"`
	BuildFiles struct {
		Patterns []string        `yaml:"patterns"`
		Ignore   []string        `yaml:"ignore"`
		Roots    map[string]Root `yaml:"roots"`
		Glob     struct {
			Exclude []string `yaml:"exclude"`
		} `yaml:"glob"`
	} `yaml:"build_files"`
	Watch struct {
		Ignore []string `yaml:"ignore"`
	} `yaml:"watch"`
	Fmt struct {
		IndentSize int `yaml:"indent_size,omitempty"`
	} `yaml:"fmt"`
	Params map[string]string `yaml:"params"`

	Extras `yaml:",inline"`

	Sources  []FileConfig `yaml:"-"`
	Profiles []string     `yaml:"-"`
}

func (c Config) OrderedPlatforms() []Platform {
	platforms := mds.Values(c.Platforms)
	slices.SortFunc(platforms, func(a, b Platform) int {
		// Order priority DESC
		return cmp.Compare(b.Priority, a.Priority)
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

	var cfg FileConfig
	dec := yaml.NewDecoder(f)
	// dec.KnownFields(true)
	err = dec.Decode(&cfg)
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
