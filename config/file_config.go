package config

type Extras map[string]interface{}

type FileConfig struct {
	BaseConfig   `yaml:",inline"`
	Cache        map[string]FileCache `yaml:",omitempty"`
	CacheHistory int                  `yaml:"cache_history"`
	DisableGC    *bool                `yaml:"disable_gc"`
	BuildFiles   struct {
		Ignore []string            `yaml:",omitempty"`
		Roots  map[string]FileRoot `yaml:",omitempty"`
	} `yaml:"build_files"`
	KeepSandbox *bool             `yaml:"keep_sandbox"`
	Params      map[string]string `yaml:"params"`
	Extras      `yaml:",inline"`
}

func (fc FileConfig) ApplyTo(c Config) Config {
	c.Sources = append(c.Sources, fc)

	if fc.Version.String != "" {
		c.Version = fc.Version
	}

	if fc.Location != "" {
		c.Location = fc.Location
	}

	if fc.KeepSandbox != nil {
		c.KeepSandbox = *fc.KeepSandbox
	}

	if fc.DisableGC != nil {
		c.DisableGC = *fc.DisableGC
	}

	if len(fc.Cache) == 0 && fc.Cache != nil {
		c.Cache = nil
	} else {
		if c.Cache == nil {
			c.Cache = map[string]Cache{}
		}

		for k, newCache := range fc.Cache {
			c.Cache[k] = newCache.ApplyTo(c.Cache[k])
		}
	}

	if fc.CacheHistory != 0 {
		c.CacheHistory = fc.CacheHistory
	}

	if c.BuildFiles.Roots == nil {
		c.BuildFiles.Roots = map[string]Root{}
	}
	for k, newRoot := range fc.BuildFiles.Roots {
		c.BuildFiles.Roots[k] = newRoot.ApplyTo(c.BuildFiles.Roots[k])
	}

	c.BuildFiles.Ignore = append(c.BuildFiles.Ignore, fc.BuildFiles.Ignore...)

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
	URI   string `yaml:"uri"`
	Read  *bool  `yaml:",omitempty"`
	Write *bool  `yaml:",omitempty"`
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
