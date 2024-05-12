package bootstrap

import (
	"errors"
	"fmt"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/targetrun"
	"github.com/hephbuild/heph/utils/xcontext"
	"github.com/hephbuild/heph/worker2"
	"go.uber.org/multierr"
	"io"
	"os"
	"strconv"
	"sync"
)

var skipPathLogFile bool

func init() {
	skipPathLogFile, _ = strconv.ParseBool(os.Getenv("HEPH_SKIP_PATH_LOG"))
}

func PrintHumanError(err error) {
	errs := worker2.CollectRootErrors(err)
	skippedCount := 0

	var contextCanceledOnce sync.Once

	logError := func(err error) {
		var sigCause xcontext.SignalCause
		if errors.As(err, &sigCause) {
			contextCanceledOnce.Do(func() {
				log.Error(sigCause)
			})
			return
		}

		log.Error(err)
	}

	for _, err := range errs {
		var terr targetrun.TargetFailed
		if errors.As(err, &terr) {
			var sigCause xcontext.SignalCause
			if errors.As(err, &sigCause) {
				contextCanceledOnce.Do(func() {
					log.Error(sigCause)
				})
				return
			}

			log.Errorf("%v failed: %v", terr.Target.Addr, terr.Err)

			logFile := terr.LogFile
			if logFile != "" {
				info, _ := os.Stat(logFile)
				if info != nil && info.Size() > 0 {
					fmt.Fprintln(log.Writer())
					f, err := os.Open(logFile)
					if err == nil {
						_, _ = io.Copy(log.Writer(), f)
						f.Close()
						fmt.Fprintln(log.Writer())
					}
					if !skipPathLogFile {
						log.Errorf("The log file can be found at %v", logFile)
					}
				}
			}

			for _, err := range multierr.Errors(terr.Err) {
				logError(err)
			}

			continue
		}

		var jerr worker2.Error
		if errors.As(err, &jerr) && jerr.Skipped() {
			skippedCount++
			log.Debugf("skipped: %v", jerr)
			continue
		}

		for _, err := range multierr.Errors(err) {
			logError(err)
		}
	}

	if len(errs) > 1 || skippedCount > 0 {
		fmt.Fprintln(log.Writer())
		skippedStr := ""
		if skippedCount > 0 {
			skippedStr = fmt.Sprintf(" %v skipped", skippedCount)
		}
		log.Errorf("%v jobs failed%v", len(errs), skippedStr)
	}
}
