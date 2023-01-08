package utils

import (
	log "github.com/sirupsen/logrus"
	"time"
)

func noop() {}

func TraceTiming(name string) func() {
	if !log.IsLevelEnabled(log.TraceLevel) {
		return noop
	}

	start := time.Now()
	log.Trace(name)

	return func() {
		log.Tracef("%v took %v", name, time.Since(start))
	}
}
