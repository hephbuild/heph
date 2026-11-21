// pluginmanifest generates a heph plugin distribution manifest (`*-plugin.json`)
// — `{ name, version, artifacts: [{ os, arch, path|url }] }` — so neither the CI
// workflow nor the devenv `gen-example` script has to hand-roll JSON.
//
// Two modes:
//
//	# one local artifact for the build host (devenv gen-example). `-host-path` is
//	# the sibling basename heph resolves against the manifest dir; `-checksum-from`
//	# points at the actual file on disk to hash (here the install dir):
//	go run . -name go -host-path heph-go-plugin.dylib \
//	    -checksum-from ~/.heph/plugins/go/heph-go-plugin.dylib \
//	    -out ~/.heph/plugins/go/heph-go-plugin.json
//
//	# every per-os/arch artifact a GitHub release holds, by URL, each checksum
//	# the asset's own digest (CI: .github/workflows/publish-plugin-manifests.sh):
//	go run . -name go -version "$V" -prefix heph-go-plugin \
//	    -from-assets assets.tsv -url-base "$BASE" -out heph-go-plugin.json
//
// `-from-assets` reads one `<name>\t<digest>` line per release asset, as the
// GitHub releases API reports them. The digest is GitHub's `sha256:<hex>` of the
// stored bytes, the same string this tool would compute from the file, so the
// manifest is built without downloading a single cdylib.
//
// `os`/`arch` use Go's runtime spelling (darwin/linux, amd64/arm64), which is
// exactly the manifest spelling heph's resolver matches against.
package main

import (
	"crypto/sha256"
	"encoding/hex"
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
	// Checksum of the artifact bytes (`sha256:<hex>`), matched by heph's resolver
	// against the downloaded/local cdylib before it is loaded.
	Checksum string `json:"checksum,omitempty"`
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
	hostPath := flag.String("host-path", "", "single-artifact mode: the artifact `path` recorded in the manifest (resolved by heph relative to the manifest dir, so normally just the sibling basename)")
	checksumFrom := flag.String("checksum-from", "", "single-artifact mode: local file to read for the checksum (defaults to -host-path). Use when the dylib lives elsewhere than the host-path the manifest should record, e.g. an install dir distinct from this tool's cwd")
	fromAssets := flag.String("from-assets", "", "release mode: file of `<name>\\t<digest>` lines, one per release asset; the <prefix>_<os>_<arch>.{so,dylib} ones become the manifest")
	prefix := flag.String("prefix", "", "release mode: artifact filename prefix (e.g. heph-go-plugin)")
	urlBase := flag.String("url-base", "", "release mode: URL base each artifact is published under")
	flag.Parse()

	if *name == "" {
		fatal("missing -name")
	}

	var arts []artifact
	switch {
	case *hostPath != "":
		// The manifest records `host-path` verbatim (heph resolves it relative to
		// the manifest dir), but the checksum must come from a file that actually
		// exists at generation time — which may be a different location (e.g. the
		// install dir, while host-path is just the sibling basename). Default the
		// checksum source to host-path for the simple same-dir case.
		csFrom := *checksumFrom
		if csFrom == "" {
			csFrom = *hostPath
		}
		arts = []artifact{{OS: runtime.GOOS, Arch: runtime.GOARCH, Path: *hostPath, Checksum: checksumFile(csFrom)}}
	case *fromAssets != "":
		if *prefix == "" || *urlBase == "" {
			fatal("-from-assets requires -prefix and -url-base")
		}
		arts = fromReleaseAssets(*fromAssets, *prefix, *urlBase)
		if len(arts) == 0 {
			fatal("no %s_<os>_<arch>.{so,dylib} assets listed in %s", *prefix, *fromAssets)
		}
	default:
		fatal("need -host-path or -from-assets")
	}

	b, err := json.MarshalIndent(manifest{Name: *name, Version: *version, Artifacts: arts}, "", "  ")
	if err != nil {
		fatal("marshal: %v", err)
	}
	b = append(b, '\n')

	// Checksum of the exact manifest bytes, for consumers to pin in their
	// `.hephconfig` as `plugins[].checksum`. Emitted as a `<out>.sha256` sidecar
	// (released alongside the manifest) and echoed to stderr.
	manifestSum := "sha256:" + hex.EncodeToString(sha256Bytes(b))
	fmt.Fprintf(os.Stderr, "pluginmanifest: manifest checksum %s\n", manifestSum)

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
	if err := os.WriteFile(*out+".sha256", []byte(manifestSum+"\n"), 0o644); err != nil {
		fatal("write %s.sha256: %v", *out, err)
	}
}

// checksumFile returns the `sha256:<hex>` digest of the file at path.
func checksumFile(path string) string {
	data, err := os.ReadFile(path)
	if err != nil {
		fatal("read %s for checksum: %v", path, err)
	}
	return "sha256:" + hex.EncodeToString(sha256Bytes(data))
}

func sha256Bytes(data []byte) []byte {
	sum := sha256.Sum256(data)
	return sum[:]
}

// fromReleaseAssets reads the `<name>\t<digest>` listing at path and maps each
// `<prefix>_<os>_<arch>.{so,dylib}` asset to a URL under base, its checksum the
// asset's digest. Sorted for a deterministic manifest: every CI job that writes
// this manifest must produce the same bytes from the same listing, since the
// publishing script decides "already published" by comparing digests. Each
// artifact keeps its native cdylib extension (Linux `.so` / macOS `.dylib`); the
// manifest records each one's URL explicitly, so the host resolver never has to
// guess the suffix.
//
// A matching asset without a `sha256:` digest is fatal rather than skipped: a
// manifest that silently dropped a platform would be published as current.
func fromReleaseAssets(path, prefix, base string) []artifact {
	data, err := os.ReadFile(path)
	if err != nil {
		fatal("read asset listing %s: %v", path, err)
	}
	base = strings.TrimRight(base, "/")
	pre := prefix + "_"
	var arts []artifact
	for _, line := range strings.Split(string(data), "\n") {
		fn, digest, _ := strings.Cut(strings.TrimRight(line, "\r"), "\t")
		mid, ok := strings.CutPrefix(fn, pre)
		if !ok {
			continue
		}
		// Native cdylib extension per OS: `.so` (Linux) / `.dylib` (macOS).
		var found bool
		for _, ext := range []string{".so", ".dylib"} {
			if mid, found = strings.CutSuffix(mid, ext); found {
				break
			}
		}
		if !found {
			continue
		}
		goos, arch, ok := strings.Cut(mid, "_")
		if !ok || goos == "" || arch == "" {
			continue
		}
		hexSum, ok := strings.CutPrefix(digest, "sha256:")
		if _, err := hex.DecodeString(hexSum); !ok || err != nil || len(hexSum) != sha256.Size*2 {
			fatal("asset %s has no sha256 digest (got %q)", fn, digest)
		}
		arts = append(arts, artifact{OS: goos, Arch: arch, URL: base + "/" + fn, Checksum: digest})
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
