package engine

type Config struct {
	Version   string
	Providers []ConfigProvider
	Drivers   []ConfigDriver
	Caches    []ConfigCache
}

type ConfigProvider struct {
	Name    string
	Enabled bool
	Options map[string]any
}

type ConfigDriver struct {
	Name    string
	Enabled bool
	Options map[string]any
}

type ConfigCache struct {
	Name    string
	Driver  string
	Read    bool
	Write   bool
	Options map[string]any
}
