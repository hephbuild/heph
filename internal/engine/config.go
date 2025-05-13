package engine

type Config struct {
	Version   string
	Providers []ConfigProvider
	Drivers   []ConfigDriver
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
