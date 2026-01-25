package hcobra

import (
	_ "embed"
	"strings"

	"github.com/spf13/cobra"
	"github.com/spf13/pflag"
)

type FlagSet struct {
	Name string
	*pflag.FlagSet
}

func NewFlagSet(name string) FlagSet {
	return FlagSet{Name: name, FlagSet: pflag.NewFlagSet(name, pflag.ContinueOnError)}
}

func (fg FlagSet) BoolStrVar(p *BoolStr, name, shorthand, usage string) {
	fg.AddFlag(NewBoolStrFlag(p, name, shorthand, usage))
}

type cmdFlagSets struct {
	local  []FlagSet
	global []FlagSet
}

var cmdToFlagSets = map[*cobra.Command]cmdFlagSets{}

func AddLocalFlagSet(cmd *cobra.Command, fg FlagSet) {
	cmd.Flags().AddFlagSet(fg.FlagSet)

	s := cmdToFlagSets[cmd]
	s.local = append(s.local, fg)
	cmdToFlagSets[cmd] = s
}

func AddPersistentFlagSet(cmd *cobra.Command, fg FlagSet) {
	cmd.PersistentFlags().AddFlagSet(fg.FlagSet)

	s := cmdToFlagSets[cmd]
	s.global = append(s.global, fg)
	cmdToFlagSets[cmd] = s
}

func renderLocalFlags(cmd *cobra.Command) string {
	if cmd == nil {
		return ""
	}

	return RenderFlags(cmd.LocalFlags(), cmdToFlagSets[cmd].local, "Flags", "Other Flags")
}

func renderGlobalFlags(cmd *cobra.Command) string {
	if cmd == nil {
		return ""
	}

	allFlagSets := make([]FlagSet, 0)

	allFlagSets = append(allFlagSets, cmdToFlagSets[cmd].global...)
	cmd.VisitParents(func(cmd *cobra.Command) {
		allFlagSets = append(allFlagSets, cmdToFlagSets[cmd].global...)
	})

	return RenderFlags(cmd.InheritedFlags(), allFlagSets, "Global Flags", "Other Global Flags")
}

func RenderFlags(cmdFlagSet *pflag.FlagSet, flagsets []FlagSet, groupNameOnly, groupNameOther string) string {
	var sb strings.Builder

	visited := map[string]struct{}{}
	for _, fs := range flagsets {
		if sb.Len() > 0 {
			sb.WriteString("\n")
		}
		sb.WriteString(fs.Name)
		sb.WriteString(":\n")
		sb.WriteString(fs.FlagUsages())

		fs.VisitAll(func(flag *pflag.Flag) {
			visited[flag.Name] = struct{}{}
		})
	}

	if len(visited) == 0 {
		if sb.Len() > 0 {
			sb.WriteString("\n")
		}
		sb.WriteString(groupNameOnly)
		sb.WriteString(":\n")
		sb.WriteString(cmdFlagSet.FlagUsages())
	} else {
		ffs := pflag.NewFlagSet("", pflag.ContinueOnError)
		cmdFlagSet.VisitAll(func(flag *pflag.Flag) {
			if _, ok := visited[flag.Name]; ok {
				return
			}

			ffs.AddFlag(flag)
		})

		if ffs.HasAvailableFlags() {
			if sb.Len() > 0 {
				sb.WriteString("\n")
			}
			sb.WriteString(groupNameOther)
			sb.WriteString(":\n")
			sb.WriteString(ffs.FlagUsages())
		}
	}

	return sb.String()
}

//go:embed usage.gotpl
var usageTemplate string

func Setup(cmd *cobra.Command) {
	cobra.AddTemplateFunc("renderLocalFlags", renderLocalFlags)
	cobra.AddTemplateFunc("renderGlobalFlags", renderGlobalFlags)

	cmd.SetUsageTemplate(usageTemplate)
}
