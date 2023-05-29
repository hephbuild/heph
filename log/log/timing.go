package log

import (
	log "github.com/hephbuild/heph/log/liblog"
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
	if !log.Default().IsLevelEnabled(log.TraceLevel) {
		return noop
	}

	start := time.Now()
	if before {
		log.Default().Trace(name)
	}

	return func() {
		log.Default().Tracef("%v took %v", name, time.Since(start))
	}
}
