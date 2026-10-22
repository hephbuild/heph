package main

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func assistantLine(tokens int) string {
	return fmt.Sprintf(`{"type":"assistant","message":{"id":"m","usage":{"input_tokens":3,"cache_read_input_tokens":%d,"cache_creation_input_tokens":%d}}}`, tokens-1003, 1000)
}

func writeTranscript(t *testing.T, path string, lines ...string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(strings.Join(lines, "\n")+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
}

func TestLastContextReadsTheLastAssistantTurn(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "t.jsonl")
	writeTranscript(t, p,
		assistantLine(100_000),
		assistantLine(250_000),
		// A tool result after it carries no usage; a user line mentioning
		// "usage" is not an assistant turn.
		`{"type":"user","message":{"content":"what is the \"usage\" here"}}`,
	)
	if got := lastContext(p); got != 250_000 {
		t.Errorf("lastContext = %d, want 250000", got)
	}
	if got := lastContext(filepath.Join(dir, "missing")); got != 0 {
		t.Errorf("missing transcript = %d, want 0", got)
	}
}

func TestLastContextSurvivesATruncatedTail(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "t.jsonl")
	// Larger than the tail read, so the seek lands mid-line.
	filler := `{"type":"user","message":{"content":"` + strings.Repeat("x", 4096) + `"}}`
	lines := []string{assistantLine(999_000)}
	for range transcriptTail / len(filler) {
		lines = append(lines, filler)
	}
	lines = append(lines, assistantLine(420_000), filler)
	writeTranscript(t, p, lines...)
	if got := lastContext(p); got != 420_000 {
		t.Errorf("lastContext = %d, want 420000", got)
	}
}

func TestContextNoticeFiresOncePerStep(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "t.jsonl")
	notice := func(tokens int) bool {
		writeTranscript(t, p, assistantLine(tokens))
		return contextNotice(p, "sess", "", dir) != ""
	}
	for _, step := range []struct {
		tokens int
		want   bool
	}{
		{150_000, false},
		{310_000, true},  // crossed 300k
		{350_000, false}, // same step
		{405_000, true},  // crossed 400k
		{120_000, false}, // compacted: re-arms
		{301_000, true},
	} {
		if got := notice(step.tokens); got != step.want {
			t.Errorf("at %d: notice = %v, want %v", step.tokens, got, step.want)
		}
	}
}

func TestContextNoticeReadsTheSubagentsOwnTranscript(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "sess.jsonl")
	// The parent is far past its threshold; the subagent's notice must
	// follow the subagent's transcript, not the parent's.
	writeTranscript(t, p, assistantLine(900_000))
	sub := filepath.Join(dir, "sess", "subagents", "agent-a1.jsonl")
	if err := os.MkdirAll(filepath.Dir(sub), 0o755); err != nil {
		t.Fatal(err)
	}
	notice := func(tokens int) string {
		writeTranscript(t, sub, assistantLine(tokens))
		return contextNotice(p, "sess", "a1", dir)
	}
	if got := notice(150_000); got != "" {
		t.Errorf("subagent at 150k got %q", got)
	}
	got := notice(210_000)
	if !strings.Contains(got, "subagent") {
		t.Errorf("subagent crossing 200k got %q, want the subagent notice", got)
	}
	if got := notice(250_000); got != "" {
		t.Errorf("same step repeated: %q", got)
	}
	if got := notice(310_000); got == "" {
		t.Error("crossing 300k did not fire again")
	}
	// The subagent's state is its own: the parent still gets its first notice.
	if got := contextNotice(p, "sess", "", dir); strings.Contains(got, "subagent") || got == "" {
		t.Errorf("parent got %q, want the session notice", got)
	}
}

func TestContextNoticeSkipsUnknowns(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "t.jsonl")
	writeTranscript(t, p, assistantLine(500_000))
	if contextNotice(p, "sess", "missing", dir) != "" {
		t.Error("a subagent with no transcript got the notice")
	}
	if contextNotice("", "sess", "", dir) != "" || contextNotice(p, "", "", dir) != "" {
		t.Error("notice without a transcript or session")
	}
}
