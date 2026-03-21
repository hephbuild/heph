# Ironic isn't it ? :) Well, one need to bootstrap the build system...

MAKEFLAGS += --always-make

location = ~/.local/bin/rheph

install-dev:
	sed "s|<HEPH_SRC_ROOT>|$(shell pwd)|g" < scripts/dev.sh > /tmp/heph
	chmod +x /tmp/heph
	mkdir -p $$(dirname $(location))
	mv /tmp/heph $(location)

install-dev-build:
	cargo build --target-dir /tmp/rheph
	mv /tmp/rheph/debug/rheph $(location)
