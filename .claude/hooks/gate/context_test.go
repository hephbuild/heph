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

func TestContextNoticeSkipsSubagentsAndUnknowns(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "t.jsonl")
	writeTranscript(t, p, assistantLine(500_000))
	if contextNotice(p, "sess", "agent-1", dir) != "" {
		t.Error("a subagent's call got the notice")
	}
	if contextNotice("", "sess", "", dir) != "" || contextNotice(p, "", "", dir) != "" {
		t.Error("notice without a transcript or session")
	}
}
