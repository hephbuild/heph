use anyhow::Context;

/// Wire protocol between the rheph main process and its supervisor child.
///
/// One ASCII line per message; either `TRACK <pgid>\n` or `UNTRACK <pgid>\n`.
/// Keep it small and parse-free at runtime so the supervisor can stay tiny.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msg {
    Track(i32),
    Untrack(i32),
}

impl Msg {
    pub fn encode(&self) -> String {
        match self {
            Msg::Track(p) => format!("TRACK {p}\n"),
            Msg::Untrack(p) => format!("UNTRACK {p}\n"),
        }
    }

    pub fn parse(line: &str) -> anyhow::Result<Self> {
        let line = line.trim_end_matches('\n');
        let (cmd, rest) = line
            .split_once(' ')
            .ok_or_else(|| anyhow::anyhow!("malformed supervisor line: {line:?}"))?;
        let pid: i32 = rest
            .parse()
            .with_context(|| format!("parse pid in {line:?}"))?;
        match cmd {
            "TRACK" => Ok(Msg::Track(pid)),
            "UNTRACK" => Ok(Msg::Untrack(pid)),
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
}
