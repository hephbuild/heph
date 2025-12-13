package main

import (
	"github.com/spf13/cobra"
)

func ValidArgsFunctionTargets(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
	targets, _, err := autocompleteInit(cmd.Context(), false)
	if err != nil {
		return nil, cobra.ShellCompDirectiveError
	}

	directive := cobra.ShellCompDirectiveNoFileComp
	_, suggestions := autocompleteTargetName(targets, toComplete)

	return suggestions, directive
}

func ValidArgsFunctionLabelsOrTargets(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
	targets, labels, err := autocompleteInit(cmd.Context(), false)
	if err != nil {
		return nil, cobra.ShellCompDirectiveError
	}

	directive := cobra.ShellCompDirectiveNoFileComp
	_, suggestions := autocompleteLabelOrTarget(targets, labels, toComplete)

	return suggestions, directive
}
