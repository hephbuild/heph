package bootstrap

import (
	"errors"
	"fmt"
	"github.com/hephbuild/heph/engine"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/worker"
	"go.uber.org/multierr"
	"os"
	"os/exec"
)

func PrintHumanError(err error) {
	errs := multierr.Errors(worker.CollectRootErrors(err))
	skippedCount := 0
	skipSpacing := true

	separate := func() {
		if skipSpacing {
			skipSpacing = false
		} else {
			fmt.Fprintln(os.Stderr)
		}
	}

	for _, err := range errs {
		separate()

		var terr engine.TargetFailedError
		if errors.As(err, &terr) {
			log.Errorf("%v failed", terr.Target.FQN)

			var lerr engine.ErrorWithLogFile
			if errors.As(err, &lerr) {
				logFile := lerr.LogFile
				info, _ := os.Stat(logFile)
				if info.Size() > 0 {
					fmt.Fprintln(os.Stderr)
					c := exec.Command("cat", logFile)
					c.Stdout = os.Stderr
					_ = c.Run()
					fmt.Fprintln(os.Stderr)
					fmt.Fprintf(os.Stderr, "The log file can be found at %v\n", logFile)
				}

				log.Error(lerr.Error())
			} else {
				for _, err := range multierr.Errors(terr) {
					skipSpacing = true
					separate()
					log.Error(err)
				}
			}
		} else {
			var jerr worker.JobError
			if errors.As(err, &jerr) && jerr.Skipped() {
				skippedCount++
				skipSpacing = true
				log.Debugf("skipped: %v", jerr)
			} else {
				log.Error(err)
			}
		}
	}

	if len(errs) > 1 || skippedCount > 0 {
		fmt.Fprintln(os.Stderr)
		skippedStr := ""
		if skippedCount > 0 {
			skippedStr = fmt.Sprintf(" %v skipped", skippedCount)
		}
		log.Errorf("%v jobs failed%v", len(errs), skippedStr)
	}
}