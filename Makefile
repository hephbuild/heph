location = ~/.local/bin/heph2

install-dev:
	sed "s|<HEPH_SRC_ROOT>|$(shell pwd)|g" < scripts/dev.sh > /tmp/heph
	chmod +x /tmp/heph
	mv /tmp/heph $(location)

gen:
	cd plugin && buf generate
	cd plugin/pluginexec && buf generate
	cd plugin/plugingroup && buf generate
