package config

type FileConfig struct {
	BaseConfig `yaml:",inline"`
	Cache      map[string]FileCache `yaml:",omitempty"`
	BuildFiles struct {
		Ignore []string `yaml:",omitempty"`
	} `yaml:"build_files"`
	KeepSandbox *bool `yaml:"keep_sandbox"`
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

	c.BuildFiles.Ignore = append(c.BuildFiles.Ignore, fc.BuildFiles.Ignore...)

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
