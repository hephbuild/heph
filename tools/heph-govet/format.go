package main

// Formatter mode: heph-govet doubles as the per-package formatter driver, so
// formatting reuses the one hermetic Go tool binary (no second ~50-min build).
// It is NOT go/analysis — it rewrites whole files — so it runs on its own code
// path, entirely separate from unitchecker.
//
// Invocation (by the heph go_format / go_format_check drivers):
//
//	HEPH_GOVET_GOLANGCI_CONFIG=/path/.golangci.yml heph-govet -format [-check] a.go b.go …
//
// -format   rewrites each file in place (the driver declares them codegen=in_place);
// -check    writes nothing, prints files that would change, exits 1 if any do.
//
// Formatters and their settings come from the `.golangci.yml` `formatters:`
// block. With none enabled we default to gofmt (the standard baseline) so a
// `format` target is useful with zero config.

import (
	"bytes"
	"flag"
	"fmt"
	"os"
	"sort"
	"strings"

	golangcigofmt "github.com/golangci/gofmt/gofmt"
	"golang.org/x/tools/imports"
	gofumpt "mvdan.cc/gofumpt/format"
)

// runFormat implements `heph-govet -format …`. args is os.Args[2:] (everything
// after the `-format` subcommand token).
func runFormat(args []string) {
	fs := flag.NewFlagSet("format", flag.ContinueOnError)
	check := fs.Bool("check", false, "report files that need formatting; do not rewrite")
	if err := fs.Parse(args); err != nil {
		fatal(err.Error())
	}
	// Expand `@listfile` response-file args (one path per line) so a package
	// with thousands of files does not overflow the OS argv limit (ARG_MAX). A
	// bare path is taken literally; only a leading `@` triggers expansion.
	files, err := expandArgFiles(fs.Args())
	if err != nil {
		fatal(err.Error())
	}

	cfg, err := loadConfig(os.Getenv(configEnvVar))
	if err != nil {
		fatal(err.Error())
	}
	enabled := enabledFormatters(cfg.Formatters.Enable)

	var unformatted []string
	for _, file := range files {
		src, err := os.ReadFile(file)
		if err != nil {
			fatal(fmt.Sprintf("read %s: %v", file, err))
		}
		out, err := formatBytes(file, src, enabled, cfg.Formatters)
		if err != nil {
			fatal(fmt.Sprintf("format %s: %v", file, err))
		}
		if bytes.Equal(src, out) {
			continue
		}
		unformatted = append(unformatted, file)
		if !*check {
			// 0o644 is irrelevant to the write of an existing file's contents,
			// but required by the signature; the driver stages writable copies.
			if err := os.WriteFile(file, out, 0o644); err != nil {
				fatal(fmt.Sprintf("write %s: %v", file, err))
			}
		}
	}

	if *check && len(unformatted) > 0 {
		sort.Strings(unformatted)
		for _, f := range unformatted {
			fmt.Println(f)
		}
		os.Exit(1)
	}
}

// expandArgFiles replaces every `@listfile` argument with the paths it contains
// (one per line, blank lines ignored); non-`@` args pass through unchanged. This
// lets the driver hand thousands of files to the formatter without hitting the
// OS argv byte limit.
func expandArgFiles(args []string) ([]string, error) {
	out := make([]string, 0, len(args))
	for _, a := range args {
		if !strings.HasPrefix(a, "@") {
			out = append(out, a)
			continue
		}
		path := strings.TrimPrefix(a, "@")
		data, err := os.ReadFile(path)
		if err != nil {
			return nil, fmt.Errorf("read arg list %s: %w", path, err)
		}
		for _, line := range strings.Split(string(data), "\n") {
			if line = strings.TrimRight(line, "\r"); line != "" {
				out = append(out, line)
			}
		}
	}
	return out, nil
}

// enabledFormatters resolves the formatter list from `formatters.enable`,
// defaulting to gofmt when none are configured. Unknown names fail loudly.
func enabledFormatters(enable []string) []string {
	if len(enable) == 0 {
		return []string{"gofmt"}
	}
	known := map[string]bool{"gofmt": true, "gofumpt": true, "goimports": true}
	var unknown []string
	for _, n := range enable {
		if !known[n] {
			unknown = append(unknown, n)
		}
	}
	if len(unknown) > 0 {
		sort.Strings(unknown)
		fatal(fmt.Sprintf(
			"unsupported formatters in %s: %v (only gofmt, gofumpt, goimports are available)",
			configEnvVar, unknown,
		))
	}
	return enable
}

// formatBytes applies the enabled formatters to one file's bytes, in a fixed
// order (imports → gofmt → gofumpt) regardless of the order they were listed, so
// the result is deterministic. Each stage takes the previous stage's output.
func formatBytes(filename string, src []byte, enabled []string, fc formattersConfig) ([]byte, error) {
	on := map[string]bool{}
	for _, n := range enabled {
		on[n] = true
	}
	out := src

	if on["goimports"] {
		// LocalPrefix drives the "local" import group; comma-separated.
		imports.LocalPrefix = strings.Join(fc.Settings.Goimports.LocalPrefixes, ",")
		// FormatOnly: sort/group/drop existing imports only. Adding missing
		// imports needs the module graph (breaks the hermetic per-package
		// sandbox), so it is intentionally not attempted here.
		res, err := imports.Process(filename, out, &imports.Options{
			FormatOnly: true,
			Comments:   true,
			TabIndent:  true,
			TabWidth:   8,
		})
		if err != nil {
			return nil, fmt.Errorf("goimports: %w", err)
		}
		out = res
	}

	if on["gofmt"] {
		var rules []golangcigofmt.RewriteRule
		for _, r := range fc.Settings.Gofmt.RewriteRules {
			rules = append(rules, golangcigofmt.RewriteRule{Pattern: r.Pattern, Replacement: r.Replacement})
		}
		res, err := golangcigofmt.Source(filename, out, golangcigofmt.Options{
			NeedSimplify: fc.Settings.Gofmt.Simplify,
			RewriteRules: rules,
		})
		if err != nil {
			return nil, fmt.Errorf("gofmt: %w", err)
		}
		out = res
	}

	if on["gofumpt"] {
		res, err := gofumpt.Source(out, gofumpt.Options{
			ModulePath: fc.Settings.Gofumpt.ModulePath,
			ExtraRules: fc.Settings.Gofumpt.ExtraRules,
		})
		if err != nil {
			return nil, fmt.Errorf("gofumpt: %w", err)
		}
		out = res
	}

	return out, nil
}
