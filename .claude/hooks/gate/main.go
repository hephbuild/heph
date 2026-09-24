// Command gate is the PreToolUse(Bash) hook that refuses the commands
// CLAUDE.md says an agent should not run reflexively:
//
//   - tst, e2e: CI runs both on every push. Across ten recent sessions they
//     ran 30 and 132 times locally anyway — e2e is a full release build each
//     time. Prefixing HEPH_FULL_SUITE=1 is the opt-in for the exceptions
//     CLAUDE.md allows ("Workflow", "e2e").
//   - sleep >= 30s in the foreground: polling CI burned ~5h of wall-clock over
//     the same sessions. gh run watch in the background wakes the session.
//   - git push / gh stack submit of a tree `lint` has not passed on: a red
//     Lint job is a whole CI round-trip to learn what `lint` says locally.
//     HEPH_PUSH_UNLINTED=1 is the opt-out. See lintstamp.go for how a
//     passing `lint` is recorded.
//
// The command is parsed with mvdan.cc/sh (shfmt's parser), so quoting,
// heredocs, command substitution and `bash -c` are read the way the shell
// reads them: `git commit -m "tst"` runs git, `echo $(tst)` runs tst.
//
// The decision goes out as PreToolUse JSON on stdout, never as an exit code:
// `go run` folds every non-zero exit into 1, which Claude Code treats as a
// non-blocking error. Anything this cannot read — bad JSON, a command the
// parser rejects (zsh-only syntax) — is allowed: a gate that misfires on a
// legitimate command costs more than one that occasionally lets a suite run.
//
// `gate lint <dir> [args...]` is the other mode: the wrapper a rewritten
// `lint` runs through (runLint).
package main

import (
	"encoding/json"
	"os"
)

type hookInput struct {
	Cwd       string         `json:"cwd"`
	ToolInput map[string]any `json:"tool_input"`
}

func main() {
	if len(os.Args) > 2 && os.Args[1] == "lint" {
		os.Exit(runLint(os.Args[2], os.Args[3:]))
	}

	var in hookInput
	if err := json.NewDecoder(os.Stdin).Decode(&in); err != nil {
		return
	}
	command, _ := in.ToolInput["command"].(string)
	background, _ := in.ToolInput["run_in_background"].(bool)

	if reason := check(command, gate{
		background: background,
		unlinted:   func() string { return lintStamp(in.Cwd) },
	}); reason != "" {
		respond(map[string]any{
			"permissionDecision":       "deny",
			"permissionDecisionReason": reason,
		})
		return
	}

	// `go run -C` put this process in the gate's own directory.
	gateDir, err := os.Getwd()
	if err != nil {
		return
	}
	if rewritten := stampLint(command, lintWrapper(gateDir)); rewritten != "" {
		in.ToolInput["command"] = rewritten
		// No permissionDecision: the rewritten command still goes through the
		// normal permission flow, exactly as the original would have.
		respond(map[string]any{"updatedInput": in.ToolInput})
	}
}

func respond(out map[string]any) {
	out["hookEventName"] = "PreToolUse"
	_ = json.NewEncoder(os.Stdout).Encode(map[string]any{"hookSpecificOutput": out})
}
