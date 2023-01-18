package cmd

import (
	_ "embed"
	"github.com/spf13/cobra"
	"heph/cmd/usage"
)

//go:embed root_usage_template.gotpl
var rootUsageTemplate string

type targetShortcut struct {
	Name        string
	FQN         string
	NamePadding int
}

type rootCmdData struct {
	*cobra.Command
	TargetShortcuts []targetShortcut
}

func (c rootCmdData) HasTargetShortcuts() bool {
	return len(c.TargetShortcuts) > 0
}

func setupRootUsage() {
	defUsage := rootCmd.UsageFunc()
	rootCmd.SetUsageFunc(func(cmd *cobra.Command) error {
		ctx := cmd.Context()

		if cmd != rootCmd {
			return defUsage(cmd)
		}

		err := engineInit(ctx)
		if err != nil {
			return err
		}

		shortcuts := Engine.GetTargetShortcuts()

		c := rootCmdData{
			Command: cmd,
		}

		longestShortcut := 0
		for _, target := range shortcuts {
			if len(target.Name) > longestShortcut {
				longestShortcut = len(target.Name)
			}
		}

		for _, target := range shortcuts {
			c.TargetShortcuts = append(c.TargetShortcuts, targetShortcut{
				Name:        target.Name,
				FQN:         target.FQN,
				NamePadding: longestShortcut,
			})
		}

		return usage.Tmpl(cmd.OutOrStderr(), rootUsageTemplate, c)
	})
}
