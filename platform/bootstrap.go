package platform

import (
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/log/log"
)

type PlatformProvider struct {
	config.Platform
	Provider
}

func Bootstrap(cfg *config.Config) []PlatformProvider {
	var platformProviders []PlatformProvider
	for _, p := range cfg.OrderedPlatforms() {
		provider, err := GetProvider(p.Provider, p.Name, p.Options)
		if err != nil {
			log.Warnf("platform: %v: %v", p.Name, err)
			continue
		}

		platformProviders = append(platformProviders, PlatformProvider{
			Platform: p,
			Provider: provider,
		})
	}

	return platformProviders
}
