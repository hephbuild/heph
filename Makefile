dev-install:
	cp dev_run.sh /tmp/heph
	sed -i "" "s|HEPH_BUILD_ROOT|$(shell pwd)|g" /tmp/heph
	sudo mv /tmp/heph /usr/local/bin/heph

install:
	go build -o /tmp/heph .
	sudo mv /tmp/heph /usr/local/bin/heph
