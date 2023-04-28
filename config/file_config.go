package config

type Extras map[string]interface{}

type FileConfig struct {
	BaseConfig   `yaml:",inline"`
	Caches       map[string]FileCache `yaml:"caches,omitempty"`
	CacheOrder   string               `yaml:"cache_order"`
	CacheHistory int                  `yaml:"cache_history"`
	Cloud        struct {
		URL     string `yaml:"url"`
		Project string `yaml:"project"`
	} `yaml:"cloud"`
	Engine struct {
		GC           *bool `yaml:"gc"`
		CacheHints   *bool `yaml:"cache_hints"`
		InstallTools *bool `yaml:"install_tools"`
		KeepSandbox  *bool `yaml:"keep_sandbox"`
	} `yaml:"engine"`
	Platforms  map[string]FilePlatform `yaml:"platforms"`
	BuildFiles struct {
		Ignore []string            `yaml:"ignore,omitempty"`
		Roots  map[string]FileRoot `yaml:"roots,omitempty"`
		Glob   struct {
			Exclude []string `yaml:"exclude"`
		} `yaml:"glob"`
	} `yaml:"build_files"`
	Watch struct {
		Ignore []string `yaml:"ignore,omitempty"`
	} `yaml:"watch"`
	Params map[string]string `yaml:"params"`
	Extras `yaml:",inline"`
}

func (fc FileConfig) ApplyTo(c Config) Config {
	c.Sources = append(c.Sources, fc)

	if fc.Version.String != "" {
		c.Version = fc.Version
	}

	if fc.Location != "" {
		c.Location = fc.Location
	}

	if fc.Cloud.URL != "" {
		c.Cloud.URL = fc.Cloud.URL
	}

	if fc.Cloud.Project != "" {
		c.Cloud.Project = fc.Cloud.Project
	}

	if fc.Engine.KeepSandbox != nil {
		c.Engine.KeepSandbox = *fc.Engine.KeepSandbox
	}

	if fc.Engine.GC != nil {
		c.Engine.GC = *fc.Engine.GC
	}

	if fc.Engine.CacheHints != nil {
		c.Engine.CacheHints = *fc.Engine.CacheHints
	}

	if fc.Engine.InstallTools != nil {
		c.Engine.InstallTools = *fc.Engine.InstallTools
	}

	if fc.CacheOrder != "" {
		c.CacheOrder = fc.CacheOrder
	}

	if len(fc.Caches) == 0 && fc.Caches != nil {
		c.Caches = nil
	} else {
		if c.Caches == nil {
			c.Caches = map[string]Cache{}
		}

		for k, newCache := range fc.Caches {
			c.Caches[k] = newCache.ApplyTo(c.Caches[k])
		}
	}

	if fc.CacheHistory != 0 {
		c.CacheHistory = fc.CacheHistory
	}

	if c.Platforms == nil {
		c.Platforms = map[string]Platform{}
	}
	for k, newPlatform := range fc.Platforms {
		pf := c.Platforms[k]
		pf = newPlatform.ApplyTo(pf)
		pf.Name = k
		c.Platforms[k] = pf
	}

	if c.BuildFiles.Roots == nil {
		c.BuildFiles.Roots = map[string]Root{}
	}
	for k, newRoot := range fc.BuildFiles.Roots {
		c.BuildFiles.Roots[k] = newRoot.ApplyTo(c.BuildFiles.Roots[k])
	}

	c.BuildFiles.Ignore = append(c.BuildFiles.Ignore, fc.BuildFiles.Ignore...)
	c.BuildFiles.Glob.Exclude = append(c.BuildFiles.Glob.Exclude, fc.BuildFiles.Glob.Exclude...)
	c.Watch.Ignore = append(c.Watch.Ignore, fc.Watch.Ignore...)

	if c.Extras == nil {
		c.Extras = map[string]interface{}{}
	}
	for k, v := range fc.Extras {
		c.Extras[k] = v
	}

	if c.Params == nil {
		c.Params = map[string]string{}
	}
	for k, v := range fc.Params {
		c.Params[k] = v
	}

	return c
}

type FileCache struct {
	URI       string `yaml:"uri"`
	Read      *bool  `yaml:",omitempty"`
	Write     *bool  `yaml:",omitempty"`
	Secondary *bool  `yaml:",omitempty"`
}

func (fc FileCache) ApplyTo(c Cache) Cache {
	if fc.URI != "" {
		c.URI = fc.URI
	}

	if fc.Read != nil {
		c.Read = *fc.Read
	}

	if fc.Write != nil {
		c.Write = *fc.Write
	}

	if fc.Secondary != nil {
		c.Secondary = *fc.Secondary
	}

	return c
}

type FileRoot struct {
	URI string `yaml:"uri"`
}

func (fc FileRoot) ApplyTo(c Root) Root {
	if fc.URI != "" {
		c.URI = fc.URI
	}

	return c
}

type FilePlatform struct {
	Provider string                 `yaml:"provider"`
	Priority *int                   `yaml:"priority"`
	Options  map[string]interface{} `yaml:"options,omitempty"`
}

func (fc FilePlatform) ApplyTo(c Platform) Platform {
	if fc.Provider != "" {
		c.Provider = fc.Provider
	}
	if fc.Priority != nil {
		c.Priority = *fc.Priority
	}
	if fc.Options != nil {
		c.Options = fc.Options
	}

	return c
}
