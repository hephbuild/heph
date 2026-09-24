// Command gate is the PreToolUse(Bash) hook that refuses the commands
// CLAUDE.md says an agent should not run reflexively:
//
//   - tst, e2e: CI runs both on every push. Across ten recent sessions they
//     ran 30 and 132 times locally anyway — e2e is a full release build each
//     time. Prefixing HEPH_FULL_SUITE=1 is the opt-in for the exceptions
//     CLAUDE.md allows ("Workflow", "e2e").
//   - sleep >= 30s in the foreground: polling CI burned ~5h of wall-clock over
//     the same sessions. gh run watch in the background wakes the session.
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
package main

import (
	"encoding/json"
	"os"
)

type hookInput struct {
	ToolInput struct {
		Command         string `json:"command"`
		RunInBackground bool   `json:"run_in_background"`
	} `json:"tool_input"`
}

func main() {
	var in hookInput
	if err := json.NewDecoder(os.Stdin).Decode(&in); err != nil {
		return
	}
	reason := check(in.ToolInput.Command, in.ToolInput.RunInBackground)
	if reason == "" {
		return
	}
	_ = json.NewEncoder(os.Stdout).Encode(map[string]any{
		"hookSpecificOutput": map[string]any{
			"hookEventName":            "PreToolUse",
			"permissionDecision":       "deny",
			"permissionDecisionReason": reason,
		},
	})
}
