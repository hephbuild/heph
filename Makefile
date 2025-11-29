# Ironic isn't it ? :) Well, one need to bootstrap the build system...

MAKEFLAGS += --always-make

location = ~/.local/bin/heph2

install-dev:
	sed "s|<HEPH_SRC_ROOT>|$(shell pwd)|g" < internal/scripts/dev.sh > /tmp/heph
	chmod +x /tmp/heph
	mkdir -p $$(dirname $(location))
	mv /tmp/heph $(location)

install-dev-build:
	go build -o $(location)

gen:
	cd lib/tref/internal && ./gen.sh
	cd plugin && buf generate
	cd plugin/plugingroup && buf generate
	cd plugin/pluginbin && buf generate
	cd plugin/plugintextfile && buf generate
	cd plugin/pluginexec && buf generate
	cd plugin/pluginnix && buf generate
	cd plugin/pluginfs && buf generate
	cd internal/hproto && buf generate
	go run go.uber.org/mock/mockgen -typed -self_package=github.com/hephbuild/heph/lib/pluginsdk -package=pluginsdk -source=lib/pluginsdk/plugin_driver.go > lib/pluginsdk/plugin_driver.mock.go
	go run go.uber.org/mock/mockgen -typed -self_package=github.com/hephbuild/heph/lib/pluginsdk -package=pluginsdk -source=lib/pluginsdk/plugin_provider.go > lib/pluginsdk/plugin_provider.mock.go
	go run go.uber.org/mock/mockgen -typed -self_package=github.com/hephbuild/heph/lib/pluginsdk -package=pluginsdk -source=lib/pluginsdk/plugin_cache.go > lib/pluginsdk/plugin_cache.mock.go
