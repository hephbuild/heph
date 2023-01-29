dev-install:
	./install.sh --dev

install:
	./install.sh --build

freeze-go-env:
	go env > /tmp/heph_source

unfreeze-go-env:
	rm /tmp/heph_source
