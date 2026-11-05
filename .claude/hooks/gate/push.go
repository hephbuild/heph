package main

// Deferred push checks. The hook runs before the command does, so for
// `lint && git commit -m x && git push` it sees the HEAD from *before* the
// commit and refuses a push that would have been fine by the time it ran. That
// cost a wasted call and a re-sequence in six sessions in the week to
// 2026-11-05, each learning it afresh.
//
// So when a command also runs something that can change the answer — `lint`
// (which writes the stamp) or a git command that moves HEAD — the hook does
// not refuse its push. It prefixes the push with a call back to this program
// (pushWrapper), which checks the stamp when the shell gets there and runs the
// push only if `lint` passed on exactly HEAD's tree then (runPush).

import (
	"fmt"
	"os"
	"slices"
	"strings"

	"mvdan.cc/sh/v3/syntax"
)

// git subcommands after which HEAD's tree may differ from before them.
var movesHead = map[string]bool{
	"commit": true, "merge": true, "rebase": true, "cherry-pick": true, "am": true,
	"revert": true, "reset": true, "pull": true, "checkout": true, "switch": true, "stash": true,
}

// decide is the hook's whole verdict on cmd: why it is refused, or "" and the
// command to run instead ("" for cmd unchanged). lintW and pushW are the
// wrappers that replace a `lint` and prefix a deferred push; either may be ""
// when the gate's directory is unknown, and then that rewrite is not made.
func decide(cmd string, g gate, lintW, pushW string) (reason, rewritten string) {
	reason = check(cmd, g)
	if reason != "" && pushW != "" && changesStamp(cmd) {
		// Every push the hook would check is now behind the wrapper and no
		// longer reads as a push. Whatever is still refused — another rule, or
		// a push the splice cannot reach, such as one inside `bash -c '…'` —
		// keeps the command refused.
		if deferred := deferPushes(cmd, pushW); deferred != "" && check(deferred, g) == "" {
			reason, rewritten = "", deferred
		}
	}
	if reason != "" {
		return reason, ""
	}
	if lintW != "" {
		base := cmd
		if rewritten != "" {
			base = rewritten
		}
		if stamped := stampLint(base, lintW); stamped != "" {
			rewritten = stamped
		}
	}
	return "", rewritten
}

// changesStamp reports whether cmd runs `lint` or a git command that moves
// HEAD — anything that can turn a push refused now into one allowed later.
func changesStamp(cmd string) bool {
	file, err := parse(cmd)
	if err != nil {
		return false
	}
	found := false
	syntax.Walk(file, func(node syntax.Node) bool {
		if call, ok := node.(*syntax.CallExpr); ok && !found {
			rest := unwrap(literals(call), map[string]bool{})
			if len(rest) > 0 && rest[0] == "lint" {
				found = true
			} else if i := gitSubcommand(rest); i > 0 && movesHead[rest[i]] {
				found = true
			}
		}
		return !found
	})
	return found
}

// pushWrapper is the prefix that defers a push's check to when it runs.
func pushWrapper(gateDir string) string {
	return "env GOTOOLCHAIN=local GOWORK=off go run -C " + shQuote(gateDir) + ` . push "$PWD"`
}

// deferPushes returns cmd with pushW put in front of each push the hook would
// check, or "" when there is none. A push opted out with HEPH_PUSH_UNLINTED=1
// is left alone; it is never checked.
func deferPushes(cmd, pushW string) string {
	return splice(cmd, func(call *syntax.CallExpr, rest []string, optIns map[string]bool) bool {
		for _, a := range call.Assigns {
			if a.Name != nil && literal(a.Value) == "1" {
				optIns[a.Name.Value] = true
			}
		}
		return pushes(rest) && !optIns[unlintedOptIn]
	}, func(word string) string { return pushW + " " + word })
}

// runPush runs args (a git or gh push) in dir only when `lint` passed on
// exactly HEAD's tree there now, and returns its exit code.
func runPush(dir string, args []string) int {
	if reason := lintStamp(dir); reason != "" {
		fmt.Fprintf(os.Stderr, "gate: push not run. %s\n", reason)
		return 1
	}
	return run(dir, args)
}

// splice returns cmd with the command word of each call pick selects replaced
// by edit(word), or "" when pick selects none. Only those words change; the
// rest of the command is kept byte for byte as written. A call inside
// `bash -c '…'` is a string to this parser, so it is never reached.
func splice(cmd string, pick func(call *syntax.CallExpr, rest []string, optIns map[string]bool) bool, edit func(word string) string) string {
	file, err := parse(cmd)
	if err != nil {
		return ""
	}
	var words []*syntax.Word
	syntax.Walk(file, func(node syntax.Node) bool {
		if call, ok := node.(*syntax.CallExpr); ok {
			args := literals(call)
			optIns := map[string]bool{}
			rest := unwrap(args, optIns)
			if len(rest) > 0 && pick(call, rest, optIns) {
				words = append(words, call.Args[len(args)-len(rest)])
			}
		}
		return true
	})
	if len(words) == 0 {
		return ""
	}
	slices.SortFunc(words, func(a, b *syntax.Word) int { return int(a.Pos().Offset()) - int(b.Pos().Offset()) })
	var b strings.Builder
	last := uint(0)
	for _, w := range words {
		b.WriteString(cmd[last:w.Pos().Offset()])
		b.WriteString(edit(cmd[w.Pos().Offset():w.End().Offset()]))
		last = w.End().Offset()
	}
	b.WriteString(cmd[last:])
	return b.String()
}
