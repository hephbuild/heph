// Command stack-status lets a stack merge on its top's run.
//
// `gh stack merge` holds every layer of a stack to the default branch's
// required status checks. The CI gate skips the middle layers, so they have
// none. Given a completed CI run on the top of a stack, this posts each required
// check's result from that run, as a commit status under the same name, on
// every middle layer whose head commit is in the tree the run built.
//
// It vouches for nothing it cannot back with that run. Nothing is posted:
//   - for a push, a fork's run, or a run of a commit the PR has since moved on
//     from;
//   - for a PR that targets the default branch, or for the bottom layer of a
//     stack: those always build and carry their own check runs;
//   - when a required check did not run, as on a middle layer's own (skipped)
//     run, or a build that stopped before its matrix expanded;
//   - on a layer whose head is not an ancestor of the commit the run built.
//
// That is not a failure, and the program exits 0. Only an API error fails it.
//
// Environment: GH_TOKEN, GH_REPO (owner/name), RUN_ID, DEFAULT_BRANCH, and
// GITHUB_API_URL (Actions sets it; https://api.github.com otherwise).
// .github/workflows/stack-status.yml runs it.
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"strings"
	"time"
)

func main() {
	env := func(k string) string {
		v := os.Getenv(k)
		if v == "" {
			fmt.Fprintf(os.Stderr, "stack-status: %s is not set\n", k)
			os.Exit(2)
		}
		return v
	}
	api := os.Getenv("GITHUB_API_URL")
	if api == "" {
		api = "https://api.github.com"
	}
	gh := &github{
		api:    strings.TrimSuffix(api, "/"),
		repo:   env("GH_REPO"),
		token:  env("GH_TOKEN"),
		client: &http.Client{Timeout: 30 * time.Second},
	}
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()
	if err := mirror(ctx, gh, env("RUN_ID"), env("DEFAULT_BRANCH"), os.Stdout); err != nil {
		fmt.Fprintf(os.Stderr, "stack-status: %v\n", err)
		os.Exit(1)
	}
}

// maxDepth bounds the walk down a stack, so a cycle of bases cannot spin.
const maxDepth = 64

// errNothing ends a mirror that has nothing to vouch with or for.
type errNothing string

func (e errNothing) Error() string { return string(e) }

// status is one required check's result in the top's run.
type status struct {
	context, state, conclusion, url string
}

// mirror posts run runID's required check results onto the middle layers of
// the stack it is the top of, and reports what it did on out as Actions
// workflow commands.
func mirror(ctx context.Context, gh *github, runID, defaultBranch string, out io.Writer) error {
	err := doMirror(ctx, gh, runID, defaultBranch, out)
	if nothing, ok := errors.AsType[errNothing](err); ok {
		fmt.Fprintf(out, "Nothing to mirror: %s\n", nothing)
		fmt.Fprintf(out, "::notice title=Stack status::%s\n", nothing)
		return nil
	}
	return err
}

func doMirror(ctx context.Context, gh *github, runID, defaultBranch string, out io.Writer) error {
	run, err := gh.run(ctx, runID)
	if err != nil {
		return err
	}
	if run.Event != "pull_request" {
		return errNothing(fmt.Sprintf("run %s is a %q run, not a pull request's.", runID, run.Event))
	}
	if run.HeadRepository.FullName != gh.repo {
		return errNothing(fmt.Sprintf("run %s is from a fork (%s): no layer of a stack in %s can be under it.", runID, run.HeadRepository.FullName, gh.repo))
	}

	top, err := gh.prOf(ctx, run.HeadBranch)
	if err != nil {
		return err
	}
	if top == nil {
		return errNothing(fmt.Sprintf("no single open PR has %q as its head.", run.HeadBranch))
	}
	if top.Head.SHA != run.HeadSHA {
		return errNothing(fmt.Sprintf("#%d has moved on to %s since run %s built %s.", top.Number, top.Head.SHA, runID, run.HeadSHA))
	}
	if top.Base.Ref == defaultBranch {
		return errNothing(fmt.Sprintf("#%d targets %q, so it carries its own checks and has no layer under it.", top.Number, defaultBranch))
	}

	// The ruleset is the list: nothing here goes stale when it changes.
	contexts, err := gh.requiredChecks(ctx, defaultBranch)
	if err != nil {
		return err
	}
	if len(contexts) == 0 {
		return errNothing(fmt.Sprintf("%q requires no status checks.", defaultBranch))
	}
	jobs, err := gh.jobs(ctx, runID)
	if err != nil {
		return err
	}
	statuses := make([]status, 0, len(contexts))
	for _, c := range contexts {
		j, ok := jobs[c]
		if !ok {
			return errNothing(fmt.Sprintf("run %s of #%d has no %q job: it did not build, so it vouches for nothing.", runID, top.Number, c))
		}
		statuses = append(statuses, status{context: c, state: stateOf(j.Conclusion), conclusion: conclusionOf(j.Conclusion), url: j.HTMLURL})
	}

	// Down the stack, base by base, to the layer that targets the default
	// branch. That one (the bottom) has its own run and is left alone.
	base := top.Base.Ref
	for range maxDepth {
		if base == defaultBranch {
			return nil
		}
		layer, err := gh.prOf(ctx, base)
		if err != nil {
			return err
		}
		if layer == nil {
			fmt.Fprintf(out, "::warning title=Stack status::no single open PR has %q as its head; the layers under it get nothing.\n", base)
			return nil
		}
		if layer.Base.Ref == defaultBranch {
			fmt.Fprintf(out, "#%d is the bottom of the stack: it builds on its own.\n", layer.Number)
			return nil
		}
		in, rel, err := gh.contains(ctx, run.HeadSHA, layer.Head.SHA)
		if err != nil {
			return err
		}
		if !in {
			fmt.Fprintf(out, "::warning title=Stack status::#%d's head %s is not in the tree #%d's run built (%s): it gets nothing until a run covers it.\n", layer.Number, layer.Head.SHA, top.Number, rel)
		} else {
			for _, s := range statuses {
				if err := gh.postStatus(ctx, layer.Head.SHA, s, fmt.Sprintf("#%d's run: %s", top.Number, s.conclusion)); err != nil {
					return err
				}
			}
			fmt.Fprintf(out, "#%d (%s): %d checks from #%d's run %s.\n", layer.Number, layer.Head.SHA, len(statuses), top.Number, runID)
		}
		base = layer.Base.Ref
	}
	fmt.Fprintf(out, "::warning title=Stack status::stopped after %d layers below #%d.\n", maxDepth, top.Number)
	return nil
}

// stateOf maps a job's conclusion (nil while it runs) to a commit status
// state. Anything but success is a failure: a red check stays red on the
// layers, so their PR pages say which run to read.
func stateOf(conclusion *string) string {
	switch {
	case conclusion == nil:
		return "pending"
	case *conclusion == "success":
		return "success"
	default:
		return "failure"
	}
}

func conclusionOf(conclusion *string) string {
	if conclusion == nil {
		return "pending"
	}
	return *conclusion
}

// github is the slice of the REST API this needs.
type github struct {
	api, repo, token string
	client           *http.Client
}

type workflowRun struct {
	Event          string `json:"event"`
	HeadSHA        string `json:"head_sha"`
	HeadBranch     string `json:"head_branch"`
	HeadRepository struct {
		FullName string `json:"full_name"`
	} `json:"head_repository"`
}

type pull struct {
	Number int `json:"number"`
	Base   struct {
		Ref string `json:"ref"`
	} `json:"base"`
	Head struct {
		SHA string `json:"sha"`
	} `json:"head"`
}

type job struct {
	Name       string  `json:"name"`
	Conclusion *string `json:"conclusion"`
	HTMLURL    string  `json:"html_url"`
}

func (g *github) run(ctx context.Context, id string) (*workflowRun, error) {
	var r workflowRun
	if err := g.get(ctx, "actions/runs/"+url.PathEscape(id), nil, &r); err != nil {
		return nil, fmt.Errorf("reading run %s: %w", id, err)
	}
	return &r, nil
}

// prOf is the open PR whose head is branch, or nil unless there is exactly one.
func (g *github) prOf(ctx context.Context, branch string) (*pull, error) {
	owner, _, _ := strings.Cut(g.repo, "/")
	var prs []pull
	q := url.Values{"state": {"open"}, "head": {owner + ":" + branch}}
	if err := g.get(ctx, "pulls", q, &prs); err != nil {
		return nil, fmt.Errorf("listing the open PRs with head %q: %w", branch, err)
	}
	if len(prs) != 1 {
		return nil, nil
	}
	return &prs[0], nil
}

func (g *github) requiredChecks(ctx context.Context, branch string) ([]string, error) {
	var rules []struct {
		Type       string `json:"type"`
		Parameters struct {
			RequiredStatusChecks []struct {
				Context string `json:"context"`
			} `json:"required_status_checks"`
		} `json:"parameters"`
	}
	if err := g.get(ctx, "rules/branches/"+url.PathEscape(branch), nil, &rules); err != nil {
		return nil, fmt.Errorf("reading the rules for %q: %w", branch, err)
	}
	var contexts []string
	for _, r := range rules {
		if r.Type != "required_status_checks" {
			continue
		}
		for _, c := range r.Parameters.RequiredStatusChecks {
			contexts = append(contexts, c.Context)
		}
	}
	return contexts, nil
}

// jobs is the latest attempt of each of the run's jobs, by name.
func (g *github) jobs(ctx context.Context, runID string) (map[string]job, error) {
	const perPage = 100
	byName := map[string]job{}
	for page := 1; ; page++ {
		var resp struct {
			Jobs []job `json:"jobs"`
		}
		q := url.Values{"filter": {"latest"}, "per_page": {fmt.Sprint(perPage)}, "page": {fmt.Sprint(page)}}
		if err := g.get(ctx, "actions/runs/"+url.PathEscape(runID)+"/jobs", q, &resp); err != nil {
			return nil, fmt.Errorf("listing run %s's jobs: %w", runID, err)
		}
		for _, j := range resp.Jobs {
			byName[j.Name] = j
		}
		if len(resp.Jobs) < perPage {
			return byName, nil
		}
	}
}

// contains reports whether commit is an ancestor of (or is) head, with the
// compare API's word for how they relate.
func (g *github) contains(ctx context.Context, head, commit string) (bool, string, error) {
	var c struct {
		Status string `json:"status"`
	}
	if err := g.get(ctx, "compare/"+url.PathEscape(commit)+"..."+url.PathEscape(head), nil, &c); err != nil {
		return false, "", fmt.Errorf("comparing %s with %s: %w", commit, head, err)
	}
	// `ahead`: head is ahead of commit and not behind it.
	return c.Status == "ahead" || c.Status == "identical", c.Status, nil
}

func (g *github) postStatus(ctx context.Context, sha string, s status, description string) error {
	body := map[string]string{
		"state":       s.state,
		"context":     s.context,
		"target_url":  s.url,
		"description": description,
	}
	if err := g.do(ctx, http.MethodPost, "statuses/"+url.PathEscape(sha), nil, body, nil); err != nil {
		return fmt.Errorf("posting %q on %s: %w", s.context, sha, err)
	}
	return nil
}

func (g *github) get(ctx context.Context, path string, q url.Values, out any) error {
	return g.do(ctx, http.MethodGet, path, q, nil, out)
}

// do calls repos/<repo>/<path>. A non-2xx answer is an error carrying the
// start of its body.
func (g *github) do(ctx context.Context, method, path string, q url.Values, body, out any) error {
	u := g.api + "/repos/" + g.repo + "/" + path
	if len(q) > 0 {
		u += "?" + q.Encode()
	}
	var reqBody io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return fmt.Errorf("encoding the request: %w", err)
		}
		reqBody = bytes.NewReader(b)
	}
	req, err := http.NewRequestWithContext(ctx, method, u, reqBody)
	if err != nil {
		return fmt.Errorf("building %s %s: %w", method, u, err)
	}
	req.Header.Set("Accept", "application/vnd.github+json")
	req.Header.Set("Authorization", "Bearer "+g.token)
	req.Header.Set("X-GitHub-Api-Version", "2022-11-28")
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	resp, err := g.client.Do(req)
	if err != nil {
		return fmt.Errorf("%s %s: %w", method, u, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode/100 != 2 {
		msg, _ := io.ReadAll(io.LimitReader(resp.Body, 512))
		return fmt.Errorf("%s %s: %s: %s", method, u, resp.Status, bytes.TrimSpace(msg))
	}
	if out == nil {
		return nil
	}
	if err := json.NewDecoder(resp.Body).Decode(out); err != nil {
		return fmt.Errorf("decoding %s %s: %w", method, u, err)
	}
	return nil
}
