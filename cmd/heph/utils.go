package main

import (
	"errors"
	"fmt"
	"github.com/spf13/cobra"
	"go.uber.org/multierr"
	"heph/cmd/heph/search"
	"heph/engine"
	log "heph/hlog"
	"heph/worker"
	"os"
	"os/exec"
)

func ValidArgsFunctionTargets(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
	targets, _, err := preRunAutocomplete(cmd.Context())
	if err != nil {
		return nil, cobra.ShellCompDirectiveError
	}

	directive := cobra.ShellCompDirectiveNoFileComp
	isFuzzy, suggestions := autocompleteTargetName(targets, toComplete)
	if isFuzzy {
		directive |= cobra.ShellCompDirectiveNoMatching
	}

	return suggestions, directive
}

func ValidArgsFunctionLabelsOrTargets(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
	targets, labels, err := preRunAutocomplete(cmd.Context())
	if err != nil {
		return nil, cobra.ShellCompDirectiveError
	}

	directive := cobra.ShellCompDirectiveNoFileComp
	isFuzzy, suggestions := autocompleteLabelOrTarget(targets, labels, toComplete)
	if isFuzzy {
		directive |= cobra.ShellCompDirectiveNoMatching
	}

	return suggestions, directive
}

func printHumanError(err error) {
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
					if printTargetNotFoundErrorSuggestions(err) {
						continue
					} else {
						log.Error(err)
					}
				}
			}
		} else {
			var jerr worker.JobError
			if errors.As(err, &jerr) && jerr.Skipped() {
				skippedCount++
				skipSpacing = true
				log.Debugf("skipped: %v", jerr)
			} else if printTargetNotFoundErrorSuggestions(err) {
				// printed in function
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

func printTargetNotFoundErrorSuggestions(err error) bool {
	e := Engine
	if e == nil {
		return false
	}

	var terr engine.TargetNotFoundErr
	if errors.As(err, &terr) {
		suggestions := search.FuzzyFindTarget(e.Targets.Specs(), terr.String, 1).FQNs()
		if len(suggestions) > 0 {
			log.Errorf("%v not found, did you mean %v ?", terr.String, suggestions[0])
			return true
		}
	}

	return false
}
