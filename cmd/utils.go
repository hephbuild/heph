package cmd

import (
	"errors"
	"fmt"
	log "github.com/sirupsen/logrus"
	"github.com/spf13/cobra"
	"go.uber.org/multierr"
	"heph/engine"
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

func humanPrintError(err error) {
	errs := multierr.Errors(err)

	for i, err := range errs {
		if i != 0 {
			fmt.Fprintln(os.Stderr)
		}

		var terr engine.TargetFailedError
		if errors.As(err, &terr) {
			log.Errorf("%v failed", terr.Target.FQN)

			var lerr engine.ErrorWithLogFile
			if errors.As(err, &lerr) {
				log.Error(lerr.Error())

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
			} else {
				log.Error(terr.Error())
			}
		} else {
			log.Error(err)
		}
	}

	if len(errs) > 1 {
		fmt.Fprintln(os.Stderr)
		log.Errorf("%v jobs failed", len(errs))
	}
}
