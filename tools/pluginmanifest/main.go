// pluginmanifest generates a heph plugin distribution manifest (`*-plugin.json`)
// — `{ name, version, artifacts: [{ os, arch, path|url }] }` — so neither the CI
// workflow nor the devenv `gen-example` script has to hand-roll JSON.
//
// Two modes:
//
//	# one local artifact for the build host (devenv gen-example):
//	go run . -name go -host-path heph-go-plugin.dylib -out .heph3/heph-go-plugin.json
//
//	# every per-os/arch artifact in a release dir, by URL (CI upload_artifacts):
//	go run . -name go -version "$V" -prefix heph-go-plugin \
//	    -from-dir dist -url-base "$BASE" -out dist/heph-go-plugin.json
//
// `os`/`arch` use Go's runtime spelling (darwin/linux, amd64/arm64), which is
// exactly the manifest spelling heph's resolver matches against.
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"sort"
	"strings"
)

type artifact struct {
	OS   string `json:"os"`
	Arch string `json:"arch"`
	Path string `json:"path,omitempty"`
	URL  string `json:"url,omitempty"`
}

type manifest struct {
	Name      string     `json:"name"`
	Version   string     `json:"version"`
	Artifacts []artifact `json:"artifacts"`
}

func main() {
	name := flag.String("name", "", "plugin name (manifest `name`)")
	version := flag.String("version", "dev", "plugin version")
	out := flag.String("out", "", "output file (default stdout)")
	hostPath := flag.String("host-path", "", "single-artifact mode: local dylib path for the build host")
	fromDir := flag.String("from-dir", "", "scan mode: dir of <prefix>_<os>_<arch>.dylib artifacts")
	prefix := flag.String("prefix", "", "scan mode: artifact filename prefix (e.g. heph-go-plugin)")
	urlBase := flag.String("url-base", "", "scan mode: URL base each artifact is published under")
	flag.Parse()

	if *name == "" {
		fatal("missing -name")
	}

	var arts []artifact
	switch {
	case *hostPath != "":
		arts = []artifact{{OS: runtime.GOOS, Arch: runtime.GOARCH, Path: *hostPath}}
	case *fromDir != "":
		if *prefix == "" || *urlBase == "" {
			fatal("-from-dir requires -prefix and -url-base")
		}
		arts = scan(*fromDir, *prefix, *urlBase)
		if len(arts) == 0 {
			fatal("no %s_<os>_<arch>.dylib artifacts found in %s", *prefix, *fromDir)
		}
	default:
		fatal("need -host-path or -from-dir")
	}

	b, err := json.MarshalIndent(manifest{Name: *name, Version: *version, Artifacts: arts}, "", "  ")
	if err != nil {
		fatal("marshal: %v", err)
	}
	b = append(b, '\n')

	if *out == "" || *out == "-" {
		os.Stdout.Write(b)
		return
	}
	if dir := filepath.Dir(*out); dir != "" {
		if err := os.MkdirAll(dir, 0o755); err != nil {
			fatal("mkdir %s: %v", dir, err)
		}
	}
	if err := os.WriteFile(*out, b, 0o644); err != nil {
		fatal("write %s: %v", *out, err)
	}
}

// scan finds `<prefix>_<os>_<arch>.dylib` files in dir and maps each to a URL
// under base. Sorted for a deterministic manifest.
//
// The `.dylib` suffix is a *uniform naming convention*, not OS-specific: CI
// publishes every cdylib — including the Linux `.so` — under a `.dylib` name
// (see .github/workflows/heph.yml) so one URL template resolves on every host.
// libloading opens by path and is extension-agnostic, so the real file format
// underneath is irrelevant. Hence matching only `.dylib` here is correct.
func scan(dir, prefix, base string) []artifact {
	entries, err := os.ReadDir(dir)
	if err != nil {
		fatal("read dir %s: %v", dir, err)
	}
	base = strings.TrimRight(base, "/")
	pre := prefix + "_"
	var arts []artifact
	for _, e := range entries {
		fn := e.Name()
		mid, ok := strings.CutPrefix(fn, pre)
		if !ok {
			continue
		}
		mid, ok = strings.CutSuffix(mid, ".dylib")
		if !ok {
			continue
		}
		goos, arch, ok := strings.Cut(mid, "_")
		if !ok || goos == "" || arch == "" {
			continue
		}
		arts = append(arts, artifact{OS: goos, Arch: arch, URL: base + "/" + fn})
	}
	sort.Slice(arts, func(i, j int) bool {
		return arts[i].OS+"/"+arts[i].Arch < arts[j].OS+"/"+arts[j].Arch
	})
	return arts
}

func fatal(format string, a ...any) {
	fmt.Fprintf(os.Stderr, "pluginmanifest: "+format+"\n", a...)
	os.Exit(1)
}
