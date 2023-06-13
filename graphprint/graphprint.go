package graphprint

import (
	"fmt"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/tgt"
	"io"
	"strings"
)

const indent = "  "

func Print(w io.Writer, target *graph.Target, transitive bool) {
	if len(target.Run) > 0 {
		fmt.Fprintln(w, "Run:")
		for _, s := range target.Run {
			fmt.Fprintln(w, indent, s)
		}
	}

	fmt.Fprintln(w, "Source:")
	for _, s := range target.Source {
		fmt.Fprintln(w, indent, s.String())
	}

	if transitive {
		if !target.OwnTransitive.Empty() {
			fmt.Fprintln(w, "Transitive:")
			printTools(w, indent, target.OwnTransitive.Tools)
			printNamedDeps(w, indent, target.OwnTransitive.Deps)
			printTransitiveEnvs(w, indent, target.OwnTransitive)
		}

		if !target.DeepOwnTransitive.Empty() {
			fmt.Fprintln(w, "Deep Transitive:")
			printTools(w, indent, target.DeepOwnTransitive.Tools)
			printNamedDeps(w, indent, target.DeepOwnTransitive.Deps)
			printTransitiveEnvs(w, indent, target.DeepOwnTransitive)
		}
	}

	if !target.EmptyDeps() {
		fmt.Fprintln(w, "Deps:")
		printTools(w, indent, target.Tools)
		printNamedDeps(w, indent, target.Deps)
		if target.DifferentHashDeps {
			fmt.Fprintln(w, "Hash Deps:")
			printDeps(w, indent, target.HashDeps)
		}
		printEnvs(w, indent, target.PassEnv, target.RuntimePassEnv, target.Env, target.RuntimeEnv)
	}

	if transitive {
		if !target.TransitiveDeps.Empty() {
			fmt.Fprintln(w, "Deps from transitive:")
			printTools(w, indent, target.TransitiveDeps.Tools)
			printNamedDeps(w, indent, target.TransitiveDeps.Deps)
			printTransitiveEnvs(w, indent, target.TransitiveDeps)
		}
	}
}

func targetDescriptor(t targetspec.TargetSpec, output string, mode targetspec.TargetSpecDepMode) string {
	var sb strings.Builder
	sb.WriteString(t.FQN)
	if len(output) > 0 {
		sb.WriteString(fmt.Sprintf("|%v", output))
	}
	if mode != "" && mode != targetspec.TargetSpecDepModeCopy {
		sb.WriteString(fmt.Sprintf(" mode=%v", mode))
	}
	return sb.String()
}

func printTargetDeps(w io.Writer, indent string, deps tgt.TargetDeps) {
	for _, t := range deps.Targets {
		fmt.Fprintln(w, indent+"  "+targetDescriptor(t.Target.Spec(), t.Output, t.Mode))
	}
}

func printNamedDeps(w io.Writer, indent string, deps tgt.TargetNamedDeps) {
	ogindent := indent

	if deps.IsNamed() {
		fmt.Fprintln(w, indent+"Deps:")

		indent := ogindent + ogindent

		for _, name := range deps.Names() {
			deps := deps.Name(name)

			if name == "" {
				name = "<>"
			}
			fmt.Fprintln(w, indent+name+":")

			indent := ogindent + ogindent + ogindent

			printDeps(w, indent, deps)
		}
	} else {
		printDeps(w, indent, deps.All())
	}
}

func printDeps(w io.Writer, indent string, deps tgt.TargetDeps) {
	if len(deps.Targets) > 0 {
		fmt.Fprintln(w, indent+"Targets:")
		printTargetDeps(w, indent, deps)
	}

	if len(deps.Files) > 0 {
		fmt.Fprintln(w, indent+"Files:")
		for _, t := range deps.Files {
			fmt.Printf(indent+"  %v\n", t.RelRoot())
		}
	}
}

func printTransitiveEnvs(w io.Writer, indent string, tr tgt.TargetTransitive) {
	printEnvs(w, indent, tr.PassEnv, tr.RuntimePassEnv, tr.Env, tr.RuntimeEnv)
}

func printEnvs(w io.Writer, indent string, passEnv, runtimePassEnv []string, env map[string]string, runtimeEnv map[string]tgt.TargetRuntimeEnv) {
	if len(passEnv) > 0 {
		fmt.Fprintln(w, indent+"Pass Env:", strings.Join(passEnv, ", "))
	}
	if len(env) > 0 {
		fmt.Fprintln(w, indent+"Env:")
		for k, v := range env {
			fmt.Printf(indent+"  %v = %v\n", k, v)
		}
	}
	if len(runtimePassEnv) > 0 {
		fmt.Fprintln(w, indent+"Runtime Pass Env:", strings.Join(runtimePassEnv, ", "))
	}
	if len(runtimeEnv) > 0 {
		fmt.Fprintln(w, indent+"Runtime Env:")
		for k, env := range runtimeEnv {
			fmt.Printf(indent+"  %v = %v\n", k, env.Value)
		}
	}
}

func printTools(w io.Writer, indent string, tools tgt.TargetTools) {
	if len(tools.Targets) > 0 {
		fmt.Fprintln(w, indent+"Tools:")
		for _, t := range tools.Targets {
			fmt.Printf(indent+"  %v\n", targetDescriptor(t.Target.Spec(), t.Output, ""))
		}
	}
	if len(tools.Hosts) > 0 {
		fmt.Fprintln(w, indent+"Host tools:")
		for _, t := range tools.Hosts {
			p, err := t.ResolvedPath()
			if err != nil {
				p = fmt.Sprintf("error: %v", err)
			}
			fmt.Printf(indent+"  %v (%v)\n", t.Name, p)
		}
	}
}
