package main

import (
	"github.com/hephbuild/heph/bootstrap"
	"github.com/spf13/cobra"
)

func ValidArgsFunctionTargets(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
	targets, _, err := preRunAutocomplete(cmd.Context(), false)
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
	targets, labels, err := preRunAutocomplete(cmd.Context(), false)
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

// Deprecated: use bootstrap.PrintHumanError
func printHumanError(err error) {
	bootstrap.PrintHumanError(err)
}
