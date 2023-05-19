package bootstrap

import (
	"errors"
	"fmt"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/engine/hroot"
	"github.com/hephbuild/heph/log/log"
	"os"
)

func BuildConfig(root *hroot.State, profiles []string) (*config.Config, error) {
	cfg := config.Config{}
	cfg.Profiles = profiles
	cfg.BuildFiles.Ignore = append(cfg.BuildFiles.Ignore, "**/.heph")
	cfg.CacheHistory = 3
	cfg.Engine.GC = true
	cfg.Engine.CacheHints = true
	cfg.CacheOrder = config.CacheOrderLatency
	cfg.Platforms = map[string]config.Platform{
		"local": {
			Name:     "local",
			Provider: "local",
			Priority: 100,
		},
	}
	cfg.Watch.Ignore = append(cfg.Watch.Ignore, root.Home.Join("**/*").Abs())

	err := config.ParseAndApply("/etc/.hephconfig", &cfg)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return nil, fmt.Errorf("/etc/.hephconfig: %w", err)
	}

	err = config.ParseAndApply(root.Root.Join(".hephconfig").Abs(), &cfg)
	if err != nil {
		return nil, fmt.Errorf(".hephconfig: %w", err)
	}

	err = config.ParseAndApply(root.Root.Join(".hephconfig.local").Abs(), &cfg)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return nil, fmt.Errorf(".hephconfig: %w", err)
	}

	log.Tracef("Profiles: %v", cfg.Profiles)

	for _, profile := range cfg.Profiles {
		err := config.ParseAndApply(root.Root.Join(".hephconfig."+profile).Abs(), &cfg)
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return nil, fmt.Errorf(".hephconfig.%v: %w", profile, err)
		}
	}

	return &cfg, nil
}
