package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"strings"
)

// Context above contextWarnAt is re-sent on every call. The notice fires once
// on crossing it, then once per contextWarnStep above it — a nudge, not a nag.
const (
	contextWarnAt   = 300_000
	contextWarnStep = 100_000
	// The transcript is read from the end; the last assistant turn is near it.
	transcriptTail = 1 << 20
)

const contextNoticeText = "Context is ~%dk tokens, and every call re-sends all of it. " +
	"At a phase or stack-layer boundary, end the turn and /compact or /handoff. " +
	"Hand a review-fix round or a rework the user asked for to a fresh agent briefed with the diff range " +
	"(isolation: \"worktree\", continued with SendMessage) instead of doing it here. " +
	"A request unrelated to the open PR is a new session. See CLAUDE.md \"Phases and hand-offs\"."

// contextNotice returns a reminder for the model when the session's context
// has crossed the next threshold since the last reminder, or "". stateDir
// holds one small file per session recording the last threshold reported;
// a /compact drops the context under it and re-arms the notice.
//
// A subagent's call is skipped: its transcript_path is not documented to be
// its own, and a subagent is the fresh context the notice points to.
func contextNotice(transcriptPath, sessionID, agentID, stateDir string) string {
	if transcriptPath == "" || sessionID == "" || agentID != "" {
		return ""
	}
	tokens := lastContext(transcriptPath)
	if tokens == 0 {
		return ""
	}
	state := filepath.Join(stateDir, "heph-gate-context-"+filepath.Base(sessionID))
	reported := 0
	if b, err := os.ReadFile(state); err == nil {
		reported, _ = strconv.Atoi(strings.TrimSpace(string(b)))
	}
	level := 0
	if tokens >= contextWarnAt {
		level = contextWarnAt + (tokens-contextWarnAt)/contextWarnStep*contextWarnStep
	}
	if level != reported {
		_ = os.WriteFile(state, []byte(strconv.Itoa(level)), 0o600)
	}
	if level == 0 || level <= reported {
		return ""
	}
	return fmt.Sprintf(contextNoticeText, tokens/1000)
}

// lastContext is the context size of the transcript's last assistant turn:
// its uncached, cache-read and cache-written input tokens. 0 when unknown.
func lastContext(path string) int {
	f, err := os.Open(path)
	if err != nil {
		return 0
	}
	defer f.Close()
	if st, err := f.Stat(); err == nil && st.Size() > transcriptTail {
		if _, err := f.Seek(-transcriptTail, io.SeekEnd); err != nil {
			return 0
		}
	}
	tail, err := io.ReadAll(f)
	if err != nil {
		return 0
	}
	lines := bytes.Split(tail, []byte("\n"))
	for i := len(lines) - 1; i >= 0; i-- {
		if !bytes.Contains(lines[i], []byte(`"usage"`)) {
			continue
		}
		var entry struct {
			Type    string `json:"type"`
			Message struct {
				Usage struct {
					Input       int `json:"input_tokens"`
					CacheRead   int `json:"cache_read_input_tokens"`
					CacheCreate int `json:"cache_creation_input_tokens"`
				} `json:"usage"`
			} `json:"message"`
		}
		// The first line of a seeked tail is usually cut short and fails here.
		if json.Unmarshal(lines[i], &entry) != nil || entry.Type != "assistant" {
			continue
		}
		u := entry.Message.Usage
		return u.Input + u.CacheRead + u.CacheCreate
	}
	return 0
}
