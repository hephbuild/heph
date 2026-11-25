//! `.github/workflows/publish-plugin-manifests.sh`, run against a fake `gh`
//! that keeps the release in a directory.
//!
//! Every `build` leg runs the script as soon as its own cdylibs are on the
//! pre-release, and the legs finish in any order, often together (the two
//! linux legs). The property under test is the one a user sees: once the legs
//! that have finished are done, every manifest lists every platform they
//! uploaded, with that asset's checksum. Darwin landing fifteen minutes later
//! adds itself to the linux entries rather than replacing them.
//!
//! The fake `gh` answers the three calls the script makes, in the shapes the
//! real API gives after the script's `--jq` (checked against a real release
//! when this was written; the fake does not run the jq program itself):
//!   - `api .../releases/tags/<v>`: the release id;
//!   - `api .../releases/<id>/assets`: one `<name>\t<digest>` line per
//!     uploaded asset, the digest empty while GitHub has none;
//!   - `release upload <v> -R <repo> --clobber <files>`: delete, then upload,
//!     with a pause in between so concurrent clobbers can collide; an upload
//!     that finds the asset recreated in that window fails, as GitHub's 422
//!     `already_exists` does.
//!
//! Faults it injects, each on the first listing of one script run
//! (`FAKE_GH_ONCE_DIR` holds the markers): a failed listing, a listing that
//! lags behind an upload, an asset listed without its digest, and a listing
//! held until the test releases it.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

const VERSION: &str = "v1.2.3-test+gabc";
const REPO: &str = "example/artifacts";
const PLUGINS: [&str; 4] = ["go", "gha", "oci", "devenv"];

const FAKE_GH: &str = r#"#!/usr/bin/env bash
set -euo pipefail
rel="$FAKE_RELEASE"
once() { [ -n "${FAKE_GH_ONCE_DIR:-}" ] && [ ! -e "$FAKE_GH_ONCE_DIR/$1" ] && : > "$FAKE_GH_ONCE_DIR/$1"; }
sha() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }
if [ "$1" = api ]; then
  for a in "$@"; do
    case "$a" in
      */releases/tags/*) echo 7; exit 0 ;;
      */assets*)
        if [ -n "${FAKE_GH_FAIL_LIST:-}" ] && once fail-list; then echo "HTTP 502" >&2; exit 1; fi
        hide=; [ -n "${FAKE_GH_HIDE:-}" ] && once hide && hide="$FAKE_GH_HIDE"
        nodigest=; [ -n "${FAKE_GH_NO_DIGEST:-}" ] && once no-digest && nodigest="$FAKE_GH_NO_DIGEST"
        for f in "$rel"/*; do
          [ -f "$f" ] || continue
          name="${f##*/}"
          if [ -n "$hide" ]; then case "$name" in *"$hide"*) continue ;; esac; fi
          if [ "$name" = "$nodigest" ]; then printf '%s\t\n' "$name"; continue; fi
          printf '%s\tsha256:%s\n' "$name" "$(sha "$f")"
        done
        # Hold this caller's first listing until the test resumes it.
        if [ -n "${FAKE_GH_STALL:-}" ] && once stall; then
          : > "$FAKE_GH_STALL"
          for _ in $(seq 1 600); do [ -e "$FAKE_GH_RESUME" ] && break; sleep 0.1; done
        fi
        exit 0 ;;
    esac
  done
  echo "fake gh: unexpected api call: $*" >&2; exit 2
fi
if [ "$1 $2" = "release upload" ]; then
  shift 3
  files=()
  while [ $# -gt 0 ]; do
    case "$1" in
      -R) shift 2 ;;
      --clobber) shift ;;
      *) files+=("$1"); shift ;;
    esac
  done
  for f in "${files[@]}"; do
    name="${f##*/}"
    case "$name" in *plugin.json*) [ -n "${FAKE_GH_UPLOAD_FAIL:-}" ] && { echo "HTTP 502" >&2; exit 1; } ;; esac
    echo "${FAKE_GH_WHO:-?} $name" >> "$FAKE_GH_LOG"
    rm -f "$rel/$name"
    sleep "0.0$((RANDOM % 10))"
    if [ -e "$rel/$name" ]; then echo "HTTP 422: already_exists" >&2; exit 1; fi
    cp "$f" "$rel/.$name.$$"
    mv "$rel/.$name.$$" "$rel/$name"
  done
  exit 0
fi
echo "fake gh: unexpected call: $*" >&2; exit 2
"#;

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/publish-plugin-manifests.sh")
}

struct Release {
    dir: tempfile::TempDir,
    bin: PathBuf,
    release: PathBuf,
    log: PathBuf,
}

impl Release {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        let release = dir.path().join("release");
        for d in [&bin, &release] {
            std::fs::create_dir(d).expect("creating the fake release's directories");
        }
        let gh = bin.join("gh");
        std::fs::write(&gh, FAKE_GH).expect("writing the fake gh");
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755))
            .expect("making the fake gh executable");
        let log = dir.path().join("uploads.log");
        Self {
            dir,
            bin,
            release,
            log,
        }
    }

    /// `program`, with the fake `gh` first on `PATH` and pointed at this
    /// release. The rest of the environment is inherited: the script builds
    /// `tools/pluginmanifest` with the host's `go`.
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut path = std::ffi::OsString::from(&self.bin);
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        let mut cmd = Command::new(program);
        cmd.env("PATH", path)
            .env("FAKE_RELEASE", &self.release)
            .env("FAKE_GH_LOG", &self.log);
        cmd
    }

    /// A fresh directory under this release's tempdir.
    fn scratch(&self, name: &str) -> PathBuf {
        let dir = self.dir.path().join(name);
        std::fs::create_dir_all(&dir).expect("creating a scratch dir");
        dir
    }

    /// What a `build` leg's "Publish to the pre-release" step does: its own
    /// four plugin cdylibs, with contents unique to this platform and `round`.
    fn upload_cdylibs(&self, platform: &str, round: u32) {
        let ext = if platform.starts_with("darwin") {
            "dylib"
        } else {
            "so"
        };
        let dir = self.scratch(&format!("staging-{platform}-{round}"));
        let mut cmd = self.command(self.bin.join("gh"));
        cmd.args(["release", "upload", VERSION, "-R", REPO, "--clobber"]);
        for plugin in PLUGINS {
            let file = dir.join(format!("heph-{plugin}-plugin_{platform}.{ext}"));
            std::fs::write(&file, format!("{plugin} {platform} round {round}"))
                .expect("writing a fake cdylib");
            cmd.arg(file);
        }
        let out = cmd.output().expect("running the fake gh");
        assert!(
            out.status.success(),
            "uploading {platform}'s cdylibs: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The script, as a leg (`expect` = its platform) or `manifests` (every
    /// platform) runs it. Each run gets its own set of one-shot faults.
    fn publish(&self, expect: &str, env: &[(&str, &str)]) -> Output {
        let once = tempfile::tempdir_in(self.dir.path()).expect("a one-shot fault dir");
        self.command("bash")
            .arg(script())
            .env("VERSION", VERSION)
            .env("REPO", REPO)
            .env("EXPECT_PLATFORMS", expect)
            .env("PUBLISH_BACKOFF_SECONDS", "0")
            .env("FAKE_GH_ONCE_DIR", once.path())
            .env("FAKE_GH_WHO", expect)
            .envs(env.iter().copied())
            .output()
            .expect("running publish-plugin-manifests.sh")
    }

    fn publish_ok(&self, expect: &str, env: &[(&str, &str)]) {
        let out = self.publish(expect, env);
        assert!(
            out.status.success(),
            "publish-plugin-manifests.sh failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// `name -> digest`, from the fake API.
    fn digests(&self) -> std::collections::BTreeMap<String, String> {
        let out = self
            .command(self.bin.join("gh"))
            .args(["api", "repos/x/releases/7/assets"])
            .output()
            .expect("listing the fake release");
        String::from_utf8(out.stdout)
            .expect("utf-8 listing")
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(n, d)| (n.to_string(), d.to_string()))
            .collect()
    }

    /// Manifest uploads so far, as `(who, asset)`.
    fn manifest_uploads(&self) -> Vec<(String, String)> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("plugin.json"))
            .filter_map(|l| l.split_once(' '))
            .map(|(w, n)| (w.to_string(), n.to_string()))
            .collect()
    }

    /// Every manifest lists exactly `platforms`, each entry the release asset's
    /// URL and digest, and each sidecar holds its manifest's digest.
    fn assert_manifests_cover(&self, platforms: &[&str]) {
        let digests = self.digests();
        let under = format!("https://github.com/{REPO}/releases/download/{VERSION}/");
        for plugin in PLUGINS {
            let name = format!("heph-{plugin}-plugin.json");
            let bytes = std::fs::read_to_string(self.release.join(&name))
                .expect("every plugin's manifest is published");
            let manifest: serde_json::Value =
                serde_json::from_str(&bytes).expect("the manifest is JSON");
            assert_eq!(text(&manifest, "name"), plugin);
            assert_eq!(text(&manifest, "version"), VERSION);
            let entries = manifest
                .get("artifacts")
                .and_then(serde_json::Value::as_array)
                .expect("an artifacts list");
            let listed: Vec<String> = entries
                .iter()
                .map(|a| format!("{}_{}", text(a, "os"), text(a, "arch")))
                .collect();
            assert_eq!(listed, platforms, "{name} lists the wrong platforms");
            for entry in entries {
                let url = text(entry, "url");
                // A URL outside this release keeps its prefix, which names no
                // asset, and fails the digest comparison below.
                let asset = url.strip_prefix(&under).unwrap_or(url);
                assert_eq!(
                    Some(text(entry, "checksum")),
                    digests.get(asset).map(String::as_str),
                    "{name}'s entry for {asset} does not carry the published asset's digest"
                );
            }
            let sidecar = std::fs::read_to_string(self.release.join(format!("{name}.sha256")))
                .expect("the .sha256 sidecar is published");
            assert_eq!(
                Some(sidecar.trim()),
                digests.get(&name).map(String::as_str),
                "{name}.sha256 does not hold the published manifest's digest"
            );
        }
    }
}

/// `v[key]` as a string, `""` when absent or not a string.
fn text<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(serde_json::Value::as_str).unwrap_or("")
}

const ALL: &str = "darwin_arm64 linux_amd64 linux_arm64";

/// A syntax slip is named as one, before the behavior tests below fail on it
/// one by one.
#[test]
fn the_script_parses() {
    let out = Command::new("bash")
        .arg("-n")
        .arg(script())
        .output()
        .expect("running bash -n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The linux legs land together and publish both linux platforms; darwin
/// lands later and adds itself; `manifests` then finds nothing to do and
/// uploads nothing.
#[test]
fn a_later_platform_adds_itself_to_the_earlier_ones() {
    let release = Arc::new(Release::new());
    let barrier = Arc::new(Barrier::new(2));
    let legs: Vec<_> = ["linux_amd64", "linux_arm64"]
        .into_iter()
        .map(|platform| {
            let (release, barrier) = (Arc::clone(&release), Arc::clone(&barrier));
            std::thread::spawn(move || {
                release.upload_cdylibs(platform, 0);
                barrier.wait();
                release.publish_ok(platform, &[]);
            })
        })
        .collect();
    for leg in legs {
        leg.join().expect("a linux leg panicked");
    }
    release.assert_manifests_cover(&["linux_amd64", "linux_arm64"]);

    release.upload_cdylibs("darwin_arm64", 0);
    release.publish_ok("darwin_arm64", &[]);
    release.assert_manifests_cover(&["darwin_arm64", "linux_amd64", "linux_arm64"]);

    let before = release.manifest_uploads().len();
    release.publish_ok(ALL, &[]);
    assert_eq!(
        release.manifest_uploads().len(),
        before,
        "a run with every manifest current uploaded again"
    );
}

/// The lost update, forced: leg A lists the release while only its own
/// platform is there and is held; leg B publishes both platforms; A is then
/// released and writes its stale manifests over B's. A single list-then-write
/// pass leaves B's platform out for good; the script must notice on its next
/// listing and put it back.
#[test]
fn a_stale_writer_does_not_drop_a_platform() {
    let release = Arc::new(Release::new());
    let control = release.scratch("control");
    let (listed, resume) = (control.join("a-listed"), control.join("a-resume"));
    release.upload_cdylibs("linux_amd64", 0);
    let a = {
        let release = Arc::clone(&release);
        let (listed, resume) = (listed.display().to_string(), resume.display().to_string());
        std::thread::spawn(move || {
            release.publish_ok(
                "linux_amd64",
                &[("FAKE_GH_STALL", &listed), ("FAKE_GH_RESUME", &resume)],
            );
        })
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    while !listed.exists() {
        if a.is_finished() {
            a.join().expect("leg A failed before listing the release");
            panic!("leg A finished without ever listing the release");
        }
        assert!(Instant::now() < deadline, "leg A never listed the release");
        std::thread::sleep(Duration::from_millis(20));
    }

    release.upload_cdylibs("linux_arm64", 0);
    release.publish_ok("linux_arm64", &[]);
    release.assert_manifests_cover(&["linux_amd64", "linux_arm64"]);

    std::fs::write(&resume, "").expect("resuming leg A");
    a.join().expect("leg A failed");
    release.assert_manifests_cover(&["linux_amd64", "linux_arm64"]);
    let by_a = release
        .manifest_uploads()
        .into_iter()
        .filter(|(who, asset)| who == "linux_amd64" && asset == "heph-go-plugin.json")
        .count();
    assert_eq!(
        by_a, 2,
        "leg A should write its stale manifest once and then repair it; the \
         interleaving this test exists for did not happen"
    );
}

/// A listing that lags behind the leg's own upload shows the earlier leg's
/// manifests, which match what that lagging listing implies. Without the
/// own-platform check the leg exits "current" with its platform missing.
#[test]
fn a_lagging_listing_is_not_taken_for_current() {
    let release = Release::new();
    release.upload_cdylibs("linux_amd64", 0);
    release.publish_ok("linux_amd64", &[]);
    release.upload_cdylibs("linux_arm64", 0);
    release.publish_ok("linux_arm64", &[("FAKE_GH_HIDE", "linux_arm64")]);
    release.assert_manifests_cover(&["linux_amd64", "linux_arm64"]);
}

/// Every leg at once, in every round, on a fresh release: whatever order the
/// clobbers collide in, every platform ends up in every manifest.
#[test]
fn legs_finishing_together_converge_on_every_platform() {
    let platforms = ["darwin_arm64", "linux_amd64", "linux_arm64"];
    for round in 0..3 {
        let release = Arc::new(Release::new());
        let barrier = Arc::new(Barrier::new(platforms.len()));
        let legs: Vec<_> = platforms
            .into_iter()
            .map(|platform| {
                let (release, barrier) = (Arc::clone(&release), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    barrier.wait();
                    release.upload_cdylibs(platform, round);
                    release.publish_ok(platform, &[]);
                })
            })
            .collect();
        for leg in legs {
            leg.join().expect("a leg panicked");
        }
        release.assert_manifests_cover(&platforms);
    }
}

/// A re-run leg uploads different bytes under the same names: the manifests
/// take the new checksums, and list the same platforms.
#[test]
fn a_rerun_leg_replaces_its_checksums() {
    let release = Release::new();
    release.upload_cdylibs("linux_amd64", 0);
    release.publish_ok("linux_amd64", &[]);
    let before = std::fs::read_to_string(release.release.join("heph-go-plugin.json"))
        .expect("the go manifest");
    release.upload_cdylibs("linux_amd64", 1);
    release.publish_ok("linux_amd64", &[]);
    release.assert_manifests_cover(&["linux_amd64"]);
    let after = std::fs::read_to_string(release.release.join("heph-go-plugin.json"))
        .expect("the go manifest");
    assert_ne!(
        before, after,
        "the re-run's checksums never reached the manifest"
    );
}

/// A cdylib listed before GitHub has its digest is retried, not published
/// without it and not fatal.
#[test]
fn a_cdylib_without_its_digest_yet_is_waited_for() {
    let release = Release::new();
    release.upload_cdylibs("linux_amd64", 0);
    release.publish_ok(
        "linux_amd64",
        &[("FAKE_GH_NO_DIGEST", "heph-go-plugin_linux_amd64.so")],
    );
    release.assert_manifests_cover(&["linux_amd64"]);
}

/// A published manifest listed without its digest yet is waited for, not
/// clobbered: re-uploading it would only restart the wait.
#[test]
fn a_manifest_without_its_digest_yet_is_not_reuploaded() {
    let release = Release::new();
    release.upload_cdylibs("linux_amd64", 0);
    release.publish_ok("linux_amd64", &[]);
    let before = release.manifest_uploads().len();
    release.publish_ok(
        "linux_amd64",
        &[("FAKE_GH_NO_DIGEST", "heph-go-plugin.json")],
    );
    assert_eq!(release.manifest_uploads().len(), before);
}

/// One failed listing is an API blip, retried, not a failed job.
#[test]
fn a_failed_listing_is_retried() {
    let release = Release::new();
    release.upload_cdylibs("linux_amd64", 0);
    release.publish_ok("linux_amd64", &[("FAKE_GH_FAIL_LIST", "1")]);
    release.assert_manifests_cover(&["linux_amd64"]);
}

/// `manifests` expects every platform: with one missing it fails, and
/// publishes no manifest that leaves it out, rather than letting the release
/// be marked complete.
#[test]
fn a_missing_platform_fails_the_backstop() {
    let release = Release::new();
    release.upload_cdylibs("linux_amd64", 0);
    release.upload_cdylibs("linux_arm64", 0);
    let out = release.publish(ALL, &[("PUBLISH_ATTEMPTS", "3")]);
    assert!(
        !out.status.success(),
        "the script passed with darwin missing"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("heph-go-plugin_darwin_arm64"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(release.manifest_uploads().is_empty());
}

/// An upload that never succeeds ends in a failure, not a loop and not a
/// success: `manifests` relies on the exit status to go red.
#[test]
fn a_release_that_never_takes_the_upload_fails_the_script() {
    let release = Release::new();
    release.upload_cdylibs("linux_amd64", 0);
    let out = release.publish(
        "linux_amd64",
        &[("FAKE_GH_UPLOAD_FAIL", "1"), ("PUBLISH_ATTEMPTS", "3")],
    );
    assert!(
        !out.status.success(),
        "the script reported success with nothing published"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("still not current after 3 attempts"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
