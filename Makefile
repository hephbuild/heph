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
