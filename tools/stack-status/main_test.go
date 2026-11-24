package main

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"slices"
	"strings"
	"sync"
	"testing"
)

// fakeGitHub serves GETs from fixtures keyed by path (plus `head` for the PR
// list), records each status posted, and answers 404 to anything else — so a
// case also proves what the mirror does *not* ask.
type fakeGitHub struct {
	t        *testing.T
	mu       sync.Mutex
	fixtures map[string]any
	posts    []post
	hits     map[string]bool
}

type post struct{ sha, context, state, description, url string }

const repo = "o/r"

// stack is a fake GitHub holding one stack: b1 (bottom, targets master) ←
// b2 ← b3 ← b4 (top), whose heads are s1…s4, and run 7, which built s4 for
// b4 and passed both required checks.
func stack(t *testing.T) *fakeGitHub {
	f := &fakeGitHub{t: t, fixtures: map[string]any{}, hits: map[string]bool{}}
	f.run(map[string]any{
		"event": "pull_request", "head_sha": "s4", "head_branch": "b4",
		"head_repository": map[string]any{"full_name": repo},
	})
	for n, base := range []string{"master", "b1", "b2", "b3"} {
		f.pr(n+1, base, fmt.Sprintf("s%d", n+1))
	}
	f.fixtures["rules/branches/master"] = []any{
		map[string]any{"type": "deletion"},
		map[string]any{"type": "required_status_checks", "parameters": map[string]any{
			"required_status_checks": []any{
				map[string]any{"context": "Test linux/amd64", "integration_id": 15368},
				map[string]any{"context": "Lint darwin/arm64", "integration_id": 15368},
			},
		}},
	}
	f.jobs(map[string]any{"Gate": "success", "Test linux/amd64": "success", "Lint darwin/arm64": "success"})
	f.compare("s2", "ahead")
	f.compare("s3", "ahead")
	return f
}

func (f *fakeGitHub) run(body any) { f.fixtures["actions/runs/7"] = body }

func (f *fakeGitHub) pr(n int, base, sha string) {
	f.fixtures[fmt.Sprintf("pulls?head=o:b%d", n)] = []any{map[string]any{
		"number": n, "base": map[string]any{"ref": base}, "head": map[string]any{"sha": sha},
	}}
}

// jobs sets run 7's jobs; a nil conclusion is a job still running.
func (f *fakeGitHub) jobs(conclusions map[string]any) {
	var jobs []any
	for name, c := range conclusions {
		jobs = append(jobs, map[string]any{"name": name, "conclusion": c, "html_url": "https://ci/" + name})
	}
	f.fixtures["actions/runs/7/jobs?page=1"] = map[string]any{"jobs": jobs}
}

func (f *fakeGitHub) compare(layer, status string) {
	f.fixtures["compare/"+layer+"...s4"] = map[string]any{"status": status}
}

func (f *fakeGitHub) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	f.mu.Lock()
	defer f.mu.Unlock()
	path, ok := strings.CutPrefix(r.URL.Path, "/repos/"+repo+"/")
	if !ok {
		http.NotFound(w, r)
		return
	}
	if r.Header.Get("Authorization") != "Bearer tok" {
		http.Error(w, "no token", http.StatusUnauthorized)
		return
	}
	if r.Method == http.MethodPost {
		sha, ok := strings.CutPrefix(path, "statuses/")
		if !ok {
			http.NotFound(w, r)
			return
		}
		var b map[string]string
		if err := json.NewDecoder(r.Body).Decode(&b); err != nil {
			http.Error(w, err.Error(), http.StatusBadRequest)
			return
		}
		f.posts = append(f.posts, post{sha, b["context"], b["state"], b["description"], b["target_url"]})
		w.WriteHeader(http.StatusCreated)
		return
	}
	key := path
	switch {
	case path == "pulls":
		if r.URL.Query().Get("state") != "open" {
			http.Error(w, "the mirror only reads open PRs", http.StatusBadRequest)
			return
		}
		key += "?head=" + r.URL.Query().Get("head")
	case strings.HasSuffix(path, "/jobs"):
		if r.URL.Query().Get("filter") != "latest" {
			http.Error(w, "the mirror reads each job's latest attempt", http.StatusBadRequest)
			return
		}
		key += "?page=" + r.URL.Query().Get("page")
	}
	body, ok := f.fixtures[key]
	if !ok {
		f.t.Errorf("unexpected GET %s", key)
		http.NotFound(w, r)
		return
	}
	f.hits[key] = true
	_ = json.NewEncoder(w).Encode(body)
}

// mirror runs the mirror for run 7 against f and returns the statuses posted
// (sorted) and what it printed.
func (f *fakeGitHub) mirror(t *testing.T) ([]post, string) {
	t.Helper()
	srv := httptest.NewServer(f)
	defer srv.Close()
	gh := &github{api: srv.URL, repo: repo, token: "tok", client: srv.Client()}
	var out strings.Builder
	if err := mirror(context.Background(), gh, "7", "master", &out); err != nil {
		t.Fatalf("mirror: %v\n%s", err, out.String())
	}
	posts := slices.Clone(f.posts)
	slices.SortFunc(posts, func(a, b post) int {
		return strings.Compare(a.sha+"\x00"+a.context, b.sha+"\x00"+b.context)
	})
	return posts, out.String()
}

func both(sha, test, lint string) []post {
	return []post{
		{sha, "Lint darwin/arm64", lint, "#4's run: " + lint, "https://ci/Lint darwin/arm64"},
		{sha, "Test linux/amd64", test, "#4's run: " + test, "https://ci/Test linux/amd64"},
	}
}

// The top's green run makes every middle layer carry each required check,
// green, and leaves the bottom (its own run) and the top (its own checks)
// alone.
func TestTheTopsRunReachesEveryMiddleLayer(t *testing.T) {
	posts, _ := stack(t).mirror(t)
	want := slices.Concat(both("s2", "success", "success"), both("s3", "success", "success"))
	if !slices.Equal(posts, want) {
		t.Fatalf("posted\n%v\nwant\n%v", posts, want)
	}
}

// A red check in the top's run is red on the layers too, not left pending:
// the PR page then says which run to read.
func TestAFailedCheckIsMirroredAsAFailure(t *testing.T) {
	f := stack(t)
	f.jobs(map[string]any{"Test linux/amd64": "failure", "Lint darwin/arm64": "success"})
	posts, _ := f.mirror(t)
	want := slices.Concat(both("s2", "failure", "success"), both("s3", "failure", "success"))
	if !slices.Equal(posts, want) {
		t.Fatalf("posted\n%v\nwant\n%v", posts, want)
	}
}

// Dispatched on a run still going: its checks are pending, and pending is
// what the layers get.
func TestAnUnfinishedCheckIsPending(t *testing.T) {
	f := stack(t)
	f.jobs(map[string]any{"Test linux/amd64": nil, "Lint darwin/arm64": "success"})
	posts, _ := f.mirror(t)
	want := slices.Concat(both("s2", "pending", "success"), both("s3", "pending", "success"))
	if !slices.Equal(posts, want) {
		t.Fatalf("posted\n%v\nwant\n%v", posts, want)
	}
}

// A layer pushed without the top rebased onto it is not in the tree that ran:
// it gets nothing, and the layer under it, which is, still does.
func TestALayerOutsideTheTopsTreeGetsNothing(t *testing.T) {
	f := stack(t)
	f.compare("s3", "diverged")
	posts, out := f.mirror(t)
	if want := both("s2", "success", "success"); !slices.Equal(posts, want) {
		t.Fatalf("posted\n%v\nwant\n%v", posts, want)
	}
	if !strings.Contains(out, "#3's head s3 is not in the tree") {
		t.Fatalf("no warning for the diverged layer:\n%s", out)
	}
}

// More than one page of jobs: a required check on the second page is found.
func TestJobsArePaged(t *testing.T) {
	f := stack(t)
	var page1 []any
	for i := range 100 {
		page1 = append(page1, map[string]any{"name": fmt.Sprintf("filler %d", i), "conclusion": "success"})
	}
	f.fixtures["actions/runs/7/jobs?page=1"] = map[string]any{"jobs": page1}
	f.fixtures["actions/runs/7/jobs?page=2"] = map[string]any{"jobs": []any{
		map[string]any{"name": "Test linux/amd64", "conclusion": "success", "html_url": "https://ci/Test linux/amd64"},
		map[string]any{"name": "Lint darwin/arm64", "conclusion": "success", "html_url": "https://ci/Lint darwin/arm64"},
	}}
	posts, _ := f.mirror(t)
	if len(posts) != 4 {
		t.Fatalf("posted %v", posts)
	}
}

// Each case where the run vouches for nothing posts nothing and succeeds: a
// red mirror job on every PR run would bury the real ones.
func TestARunThatVouchesForNothingPostsNothing(t *testing.T) {
	cases := []struct {
		what  string
		setup func(*fakeGitHub)
	}{
		{"a middle layer's own run, whose matrix jobs never expanded", func(f *fakeGitHub) {
			f.jobs(map[string]any{
				"Gate": "success",
				"Test ${{ matrix.os }}/${{ matrix.arch }}": "skipped",
				"Lint ${{ matrix.os }}/${{ matrix.arch }}": "skipped",
			})
		}},
		{"a run of a commit the top has since moved on from", func(f *fakeGitHub) { f.pr(4, "b3", "s4-new") }},
		{"a PR that targets the default branch", func(f *fakeGitHub) { f.pr(4, "master", "s4") }},
		{"a push to master", func(f *fakeGitHub) {
			f.run(map[string]any{"event": "push", "head_sha": "s4", "head_branch": "master",
				"head_repository": map[string]any{"full_name": repo}})
		}},
		{"a fork's run", func(f *fakeGitHub) {
			f.run(map[string]any{"event": "pull_request", "head_sha": "s4", "head_branch": "b4",
				"head_repository": map[string]any{"full_name": "fork/r"}})
		}},
		{"a default branch with no required checks", func(f *fakeGitHub) {
			f.fixtures["rules/branches/master"] = []any{map[string]any{"type": "deletion"}}
		}},
		{"a head branch with two open PRs", func(f *fakeGitHub) {
			f.fixtures["pulls?head=o:b4"] = []any{
				map[string]any{"number": 4, "base": map[string]any{"ref": "b3"}, "head": map[string]any{"sha": "s4"}},
				map[string]any{"number": 9, "base": map[string]any{"ref": "b2"}, "head": map[string]any{"sha": "s4"}},
			}
		}},
	}
	for _, c := range cases {
		t.Run(c.what, func(t *testing.T) {
			f := stack(t)
			c.setup(f)
			posts, out := f.mirror(t)
			if len(posts) != 0 {
				t.Fatalf("posted %v", posts)
			}
			if !strings.Contains(out, "Nothing to mirror") || !strings.Contains(out, "::notice") {
				t.Fatalf("no notice saying why:\n%s", out)
			}
		})
	}
}

// An API error fails the mirror rather than reading as "nothing to mirror".
func TestAnAPIErrorFails(t *testing.T) {
	f := stack(t)
	delete(f.fixtures, "compare/s3...s4")
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if strings.Contains(r.URL.Path, "/compare/") {
			http.Error(w, "boom", http.StatusBadGateway)
			return
		}
		f.ServeHTTP(w, r)
	}))
	defer srv.Close()
	gh := &github{api: srv.URL, repo: repo, token: "tok", client: srv.Client()}
	var out strings.Builder
	err := mirror(context.Background(), gh, "7", "master", &out)
	if err == nil || !strings.Contains(err.Error(), "502") {
		t.Fatalf("mirror = %v, want the 502\n%s", err, out.String())
	}
}
