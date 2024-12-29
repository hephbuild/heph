package hinstance

import (
	"fmt"
	"os"
	"time"
)

func gen() string {
	host, _ := os.Hostname()
	return fmt.Sprintf("%v_%v_%v", os.Getpid(), host, time.Now().Nanosecond())
}

var UID = gen()
