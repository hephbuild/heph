package platform

import "fmt"

type ProviderFactory func(name string, options map[string]interface{}) (Provider, error)

var factories = map[string]ProviderFactory{}

func RegisterProvider(name string, factory ProviderFactory) {
	factories[name] = factory
}

func GetProvider(provider, name string, options map[string]interface{}) (Provider, error) {
	f, ok := factories[provider]
	if !ok {
		return nil, fmt.Errorf("provider does not exist")
	}

	return f(name, options)
}
