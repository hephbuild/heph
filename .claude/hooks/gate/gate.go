package main

import (
	"fmt"
	"strconv"
	"strings"
	"time"

	"mvdan.cc/sh/v3/syntax"
)

// Prefix assignments, set to 1, that let a refused command through.
const (
	optIn         = "HEPH_FULL_SUITE"
	unlintedOptIn = "HEPH_PUSH_UNLINTED"
)

const tstReason = "Blocked: `tst` is the full suite, and CI runs it on every push.\n" +
	"Run the tests for what you changed: `cargo test -p <crate> <name>`, then push.\n" +
	"Only a large blast-radius change (engine core, provider/driver traits, caching) runs it locally, before opening the PR — " +
	"if that is this change, re-run as `HEPH_FULL_SUITE=1 tst`. See CLAUDE.md \"Workflow\"."

const e2eReason = "Blocked: `e2e` does a full --release build, and the bin_e2e CI job runs it on all three platforms on every push.\n" +
	"Run it locally only when changing what it covers — the plugin loader, the TUI, CLI exit codes, or the e2e script itself.\n" +
	"If that is this change, re-run as `HEPH_FULL_SUITE=1 e2e ...` (and check the `running N tests` count). See CLAUDE.md \"e2e\"."

const sleepReason = "Blocked: foreground `sleep %s`. Don't poll.\n" +
	"Waiting on CI: `gh run watch <run-id> --exit-status` with run_in_background — the session is woken when it exits.\n" +
	"Waiting on a local process: run it with run_in_background, or use Monitor with an until-loop."

// Wrappers that run their arguments as a command. The value is how many
// arguments of their own they take before the command starts.
var wrappers = map[string]int{
	"command": 0, "exec": 0, "nohup": 0, "nice": 0, "env": 0, "timeout": 1,
}

// gate is what a check needs besides the command itself.
type gate struct {
	background bool
	// unlinted says why HEAD should not be pushed yet, or "" when `lint`
	// passed on exactly HEAD's tree. Called only for a command that pushes;
	// nil allows every push.
	unlinted func() string
}

// check returns why cmd is refused, or "" to allow it.
func check(cmd string, g gate) string {
	file, err := syntax.NewParser(syntax.Variant(syntax.LangBash)).Parse(strings.NewReader(cmd), "")
	if err != nil {
		return ""
	}
	reason := ""
	syntax.Walk(file, func(node syntax.Node) bool {
		if reason != "" {
			return false
		}
		if call, ok := node.(*syntax.CallExpr); ok {
			reason = checkCall(call, g)
		}
		return true
	})
	return reason
}

func checkCall(call *syntax.CallExpr, g gate) string {
	optIns := map[string]bool{}
	for _, a := range call.Assigns {
		if a.Name != nil && literal(a.Value) == "1" {
			optIns[a.Name.Value] = true
		}
	}

	args := make([]string, 0, len(call.Args))
	for _, w := range call.Args {
		args = append(args, literal(w))
	}
	args = unwrap(args, optIns)
	if len(args) == 0 {
		return ""
	}

	switch args[0] {
	case "tst":
		if !optIns[optIn] {
			return tstReason
		}
	case "e2e":
		if !optIns[optIn] {
			return e2eReason
		}
	case "sleep":
		if !g.background && len(args) > 1 {
			if d, ok := sleepDuration(args[1:]); ok && d >= 30*time.Second {
				return fmt.Sprintf(sleepReason, strings.Join(args[1:], " "))
			}
		}
	case "git", "gh":
		if pushes(args) && !optIns[unlintedOptIn] && g.unlinted != nil {
			return g.unlinted()
		}
	case "bash", "sh", "zsh":
		// `bash -c '<script>'` runs the script: check it as a command line.
		for i, a := range args[:len(args)-1] {
			if a == "-c" || (strings.HasPrefix(a, "-") && strings.HasSuffix(a, "c") && !strings.HasPrefix(a, "--")) {
				return check(args[i+1], g)
			}
		}
	}
	return ""
}

// pushes reports whether a git or gh command line pushes commits: `git push`
// (not a branch or tag deletion) and `gh stack submit`. `gh stack sync` is
// left alone — it rebases onto a fresh trunk and pushes in one step, so no
// lint could have seen its tree.
func pushes(args []string) bool {
	switch args[0] {
	case "git":
		// Skip git's global options; -C and -c take a value.
		i := 1
		for i < len(args) && strings.HasPrefix(args[i], "-") {
			if args[i] == "-C" || args[i] == "-c" {
				i++
			}
			i++
		}
		if i >= len(args) || args[i] != "push" {
			return false
		}
		for _, a := range args[i+1:] {
			if a == "--delete" || a == "-d" || a == "--tags" {
				return false
			}
		}
		return true
	case "gh":
		return len(args) > 2 && args[1] == "stack" && args[2] == "submit"
	}
	return false
}

// unwrap strips wrappers that still run the command after them — `timeout
// 600 tst`, `env X=1 tst`, `devenv shell -- e2e` — collecting opt-in
// assignments given to env on the way.
func unwrap(args []string, optIns map[string]bool) []string {
	for len(args) > 0 {
		if n, ok := wrappers[args[0]]; ok {
			args = args[1:]
			for len(args) > 0 && strings.HasPrefix(args[0], "-") {
				args = args[1:]
			}
			if len(args) < n {
				return nil
			}
			args = args[n:]
			for len(args) > 0 && strings.Contains(args[0], "=") {
				if name, value, _ := strings.Cut(args[0], "="); value == "1" {
					optIns[name] = true
				}
				args = args[1:]
			}
			continue
		}
		if args[0] == "devenv" && len(args) > 1 && args[1] == "shell" {
			args = args[2:]
			if len(args) > 0 && args[0] == "--" {
				args = args[1:]
			}
			continue
		}
		break
	}
	return args
}

// literal returns a word's value when it is fixed text — bare, 'single' or
// "double" quoted without expansions — and "" when it depends on expansion.
func literal(w *syntax.Word) string {
	if w == nil {
		return ""
	}
	var b strings.Builder
	for _, part := range w.Parts {
		switch p := part.(type) {
		case *syntax.Lit:
			b.WriteString(p.Value)
		case *syntax.SglQuoted:
			b.WriteString(p.Value)
		case *syntax.DblQuoted:
			for _, q := range p.Parts {
				lit, ok := q.(*syntax.Lit)
				if !ok {
					return ""
				}
				b.WriteString(lit.Value)
			}
		default:
			return ""
		}
	}
	return b.String()
}

// sleepDuration sums sleep's operands, which GNU sleep accepts with an
// s/m/h/d suffix and a fractional part. ok is false for anything else.
func sleepDuration(operands []string) (time.Duration, bool) {
	units := map[byte]float64{'s': 1, 'm': 60, 'h': 3600, 'd': 86400}
	var total time.Duration
	for _, op := range operands {
		mult := 1.0
		if op != "" {
			if u, ok := units[op[len(op)-1]]; ok {
				mult, op = u, op[:len(op)-1]
			}
		}
		n, err := strconv.ParseFloat(op, 64)
		if err != nil || n < 0 {
			return 0, false
		}
		total += time.Duration(n * mult * float64(time.Second))
	}
	return total, true
}
