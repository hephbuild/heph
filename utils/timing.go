package utils

import (
	log "github.com/sirupsen/logrus"
	"time"
)

func noop() {}

func TraceTiming(name string) func() {
	return traceTiming(name, true)
}

func TraceTimingDone(name string) func() {
	return traceTiming(name, false)
}

func traceTiming(name string, before bool) func() {
	if !log.IsLevelEnabled(log.TraceLevel) {
		return noop
	}

	start := time.Now()
	if before {
		log.Trace(name)
	}

	return func() {
		log.Tracef("%v took %v", name, time.Since(start))
	}
}
