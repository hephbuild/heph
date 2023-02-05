package main

import (
	log "heph/hlog"
	lwl "heph/linux-wrap-lib"
	"os"
	"path/filepath"
)

func Execute() error {
	file, err := filepath.Abs(os.Args[1])
	if err != nil {
		return err
	}

	return lwl.Execute(file, os.Args[2:])
}

func main() {
	log.Setup()
	defer log.Cleanup()

	err := Execute()
	if err != nil {
		log.Error(err)
	}
}
