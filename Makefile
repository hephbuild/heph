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
	cd plugin && buf generate
	cd plugin/pluginexec && buf generate
	cd plugin/pluginfs && buf generate
	cd plugin/plugingroup && buf generate
	cd internal/hproto && buf generate
	go run go.uber.org/mock/mockgen -typed -self_package=github.com/hephbuild/heph/lib/engine -package=engine -source=lib/engine/plugin_driver.go > lib/engine/plugin_driver.mock.go
	go run go.uber.org/mock/mockgen -typed -self_package=github.com/hephbuild/heph/lib/engine -package=engine -source=lib/engine/plugin_provider.go > lib/engine/plugin_provider.mock.go
	go run go.uber.org/mock/mockgen -typed -self_package=github.com/hephbuild/heph/lib/engine -package=engine -source=lib/engine/plugin_cache.go > lib/engine/plugin_cache.mock.go
