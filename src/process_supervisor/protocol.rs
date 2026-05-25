use anyhow::Context;
use std::path::PathBuf;

/// Wire protocol between the rheph main process and its supervisor child.
///
/// One ASCII line per message:
///   * `TRACK <pgid>\n`     — register child pgid for SIGKILL on EOF
///   * `UNTRACK <pgid>\n`   — release a pgid
///   * `FUSEROOT <path>\n`  — register a sandboxfuse root dir; on EOF
///     the supervisor force-umounts `<path>/lower` so wedged FUSE mounts
///     don't survive a crash.
///
/// Keep it small and parse-free at runtime so the supervisor can stay tiny.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    Track(i32),
    Untrack(i32),
    FuseRoot(PathBuf),
}

impl Msg {
    pub fn encode(&self) -> String {
        match self {
            Msg::Track(p) => format!("TRACK {p}\n"),
            Msg::Untrack(p) => format!("UNTRACK {p}\n"),
            // Paths with newlines would break the line protocol. We
            // never produce such paths (sandboxfuse root is derived
            // from `home_dir`, which the engine controls), so reject
            // them defensively at encode time via a debug assert.
            Msg::FuseRoot(path) => {
                debug_assert!(!path.to_string_lossy().contains('\n'));
                format!("FUSEROOT {}\n", path.display())
            }
        }
    }

    pub fn parse(line: &str) -> anyhow::Result<Self> {
        let line = line.trim_end_matches('\n');
        let (cmd, rest) = line
            .split_once(' ')
            .ok_or_else(|| anyhow::anyhow!("malformed supervisor line: {line:?}"))?;
        match cmd {
            "TRACK" => {
                let pid: i32 = rest
                    .parse()
                    .with_context(|| format!("parse pid in {line:?}"))?;
                Ok(Msg::Track(pid))
            }
            "UNTRACK" => {
                let pid: i32 = rest
                    .parse()
                    .with_context(|| format!("parse pid in {line:?}"))?;
                Ok(Msg::Untrack(pid))
            }
            "FUSEROOT" => Ok(Msg::FuseRoot(PathBuf::from(rest))),
            other => anyhow::bail!("unknown supervisor command {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_track() {
        let m = Msg::Track(12345);
        let encoded = m.encode();
        assert_eq!(encoded, "TRACK 12345\n");
        let parsed = Msg::parse(&encoded).expect("parse");
        assert_eq!(parsed, m);
    }

    #[test]
    fn roundtrip_untrack() {
        let m = Msg::Untrack(67890);
        let encoded = m.encode();
        assert_eq!(encoded, "UNTRACK 67890\n");
        let parsed = Msg::parse(&encoded).expect("parse");
        assert_eq!(parsed, m);
    }

    #[test]
    fn rejects_unknown_command() {
        Msg::parse("BOGUS 1\n").unwrap_err();
    }

    #[test]
    fn rejects_missing_pid() {
        Msg::parse("TRACK\n").unwrap_err();
    }

    #[test]
    fn roundtrip_fuse_root() {
        let m = Msg::FuseRoot(PathBuf::from("/tmp/sandboxfuse9096"));
        let encoded = m.encode();
        assert_eq!(encoded, "FUSEROOT /tmp/sandboxfuse9096\n");
        let parsed = Msg::parse(&encoded).expect("parse");
        assert_eq!(parsed, m);
    }

    #[test]
    fn fuse_root_path_with_spaces() {
        let m = Msg::FuseRoot(PathBuf::from("/tmp/dir with space/sandboxfuse1"));
        let encoded = m.encode();
        let parsed = Msg::parse(&encoded).expect("parse");
        assert_eq!(parsed, m);
    }
}
