package bootstrap

import (
	"errors"
	"fmt"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/log/log"
	"os"
	"time"
)

func BuildConfig(root *hroot.State, profiles []string) (*config.Config, error) {
	cfg := config.Config{}
	cfg.Profiles = profiles
	cfg.BuildFiles.Ignore = append(cfg.BuildFiles.Ignore, root.Home.Abs())
	cfg.CacheHistory = 3
	cfg.Engine.GC = true
	cfg.Engine.CacheHints = true
	cfg.Engine.GitCacheHints = false
	cfg.ProgressInterval = time.Second
	cfg.Engine.ParallelCaching = true
	cfg.Engine.SmartGen = true
	cfg.CacheOrder = config.CacheOrderLatency
	cfg.BuildFiles.Patterns = []string{"**/{BUILD,*.BUILD}"}
	cfg.Platforms = map[string]config.Platform{
		"local": {
			Name:     "local",
			Provider: "local",
			Priority: 100,
		},
	}
	cfg.Watch.Ignore = append(cfg.Watch.Ignore, root.Home.Abs())
	cfg.Fmt.IndentSize = 4

	err := config.ParseAndApply("/etc/.hephconfig", &cfg)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return nil, fmt.Errorf("/etc/.hephconfig: %w", err)
	}

	err = config.ParseAndApply(root.Root.Join(".hephconfig").Abs(), &cfg)
	if err != nil {
		return nil, fmt.Errorf(".hephconfig: %w", err)
	}

	log.Tracef("Profiles: %v", cfg.Profiles)

	for _, profile := range append([]string{"local"}, cfg.Profiles...) {
		err := config.ParseAndApply(root.Root.Join(".hephconfig."+profile).Abs(), &cfg)
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return nil, fmt.Errorf(".hephconfig.%v: %w", profile, err)
		}
	}

	if len(cfg.BuildFiles.Patterns) != 1 {
		return nil, fmt.Errorf("buildfiles.patterns must have a single value")
	}

	return &cfg, nil
}
