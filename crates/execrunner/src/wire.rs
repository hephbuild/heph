//! Byte encoding for a `prepare` call crossing the plugin ABI.
//!
//! A plugin cdylib links its **own** copy of this crate, so its runner registry
//! is empty however many hosts the host binary installed — statics are not
//! shared across a dylib boundary. The plugin is therefore handed a handle back
//! to the host's registry at load time (`heph_plugin_set_runner_host`), and a
//! `prepare` from a plugin driver travels over it.
//!
//! Why bytes rather than a stabby struct: the payload is mostly `OsString`, and
//! the whole point of `SpecRewrite` holding `OsString` is that a program name,
//! an argument or an environment value may not be UTF-8. A struct of
//! `SVec<SVec<u8>>` would say the same thing with more ABI surface to freeze, so
//! the ABI method is one `SVec<u8>` in, one out, and the shape lives here where
//! both sides of the seam can see it.
//!
//! Both sides link *different copies* of this module. That is sound because the
//! format is frozen alongside `ABI_SEMVER`: a plugin built against a different
//! ABI is rejected at load, before anything is encoded.
//!
//! Unix-only, like the rest of the crate — the supported targets are three unix
//! triples, and `OsStrExt` is how an `OsString` becomes bytes without a lossy
//! round-trip through `String`.

use crate::{PrepareOutcome, SpecRewrite};
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::PathBuf;

/// A `prepare` as it crosses the seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareRequest {
    pub request_id: String,
    /// The runner target's address, formatted (`//pkg:name`).
    pub addr: String,
    pub rewrite: SpecRewrite,
}

/// The reply. An error crosses as a message rather than a type: the plugin
/// re-raises it as an `anyhow` error, and nothing on either side matches on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareReply {
    Ok {
        rewrite: SpecRewrite,
        supplies_environment: bool,
    },
    Err(String),
}

fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    // Length-prefixed, so a value containing any byte — including NUL — round
    // trips. `u32` is ample: the largest field here is an argv entry, bounded by
    // `ARG_MAX` long before it reaches 4 GiB.
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn put_os(out: &mut Vec<u8>, s: &OsString) {
    put(out, s.as_bytes());
}

fn put_u32(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&(n as u32).to_le_bytes());
}

struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, at: 0 }
    }

    fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| anyhow::anyhow!("exec-runner wire: length overflow"))?;
        let slice = self
            .buf
            .get(self.at..end)
            .ok_or_else(|| anyhow::anyhow!("exec-runner wire: truncated frame"))?;
        self.at = end;
        Ok(slice)
    }

    fn u32(&mut self) -> anyhow::Result<usize> {
        let raw: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_e| anyhow::anyhow!("exec-runner wire: short length prefix"))?;
        Ok(u32::from_le_bytes(raw) as usize)
    }

    fn bytes(&mut self) -> anyhow::Result<&'a [u8]> {
        let n = self.u32()?;
        self.take(n)
    }

    fn os(&mut self) -> anyhow::Result<OsString> {
        Ok(OsString::from_vec(self.bytes()?.to_vec()))
    }

    fn string(&mut self) -> anyhow::Result<String> {
        String::from_utf8(self.bytes()?.to_vec())
            .map_err(|e| anyhow::anyhow!("exec-runner wire: field is not utf8: {e}"))
    }

    fn path(&mut self) -> anyhow::Result<PathBuf> {
        Ok(PathBuf::from(self.os()?))
    }

    fn bool(&mut self) -> anyhow::Result<bool> {
        Ok(self.take(1)?.first().copied().unwrap_or(0) != 0)
    }
}

fn put_rewrite(out: &mut Vec<u8>, r: &SpecRewrite) {
    put(out, r.program.as_os_str().as_bytes());
    put_u32(out, r.args.len());
    for a in &r.args {
        put_os(out, a);
    }
    put_u32(out, r.env.len());
    for (k, v) in &r.env {
        put_os(out, k);
        put_os(out, v);
    }
    put(out, r.cwd.as_os_str().as_bytes());
}

fn read_rewrite(r: &mut Reader<'_>) -> anyhow::Result<SpecRewrite> {
    let program = r.path()?;
    let n = r.u32()?;
    let mut args = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        args.push(r.os()?);
    }
    let n = r.u32()?;
    let mut env = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        let k = r.os()?;
        let v = r.os()?;
        env.push((k, v));
    }
    let cwd = r.path()?;
    Ok(SpecRewrite {
        program,
        args,
        env,
        cwd,
    })
}

impl PrepareRequest {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        put(&mut out, self.request_id.as_bytes());
        put(&mut out, self.addr.as_bytes());
        put_rewrite(&mut out, &self.rewrite);
        out
    }

    pub fn decode(buf: &[u8]) -> anyhow::Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            request_id: r.string()?,
            addr: r.string()?,
            rewrite: read_rewrite(&mut r)?,
        })
    }
}

impl PrepareReply {
    #[must_use]
    pub fn ok(outcome: PrepareOutcome) -> Self {
        Self::Ok {
            rewrite: outcome.rewrite,
            supplies_environment: outcome.supplies_environment,
        }
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        match self {
            Self::Ok {
                rewrite,
                supplies_environment,
            } => {
                out.push(1);
                put_rewrite(&mut out, rewrite);
                out.push(u8::from(*supplies_environment));
            }
            Self::Err(msg) => {
                out.push(0);
                put(&mut out, msg.as_bytes());
            }
        }
        out
    }

    pub fn decode(buf: &[u8]) -> anyhow::Result<Self> {
        let mut r = Reader::new(buf);
        if r.bool()? {
            let rewrite = read_rewrite(&mut r)?;
            let supplies_environment = r.bool()?;
            Ok(Self::Ok {
                rewrite,
                supplies_environment,
            })
        } else {
            Ok(Self::Err(r.string()?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite() -> SpecRewrite {
        SpecRewrite {
            program: PathBuf::from("/usr/bin/env"),
            args: vec![OsString::from("a"), OsString::from("b c")],
            env: vec![(OsString::from("K"), OsString::from("v"))],
            cwd: PathBuf::from("/tmp/x"),
        }
    }

    #[test]
    fn a_request_round_trips() {
        let req = PrepareRequest {
            request_id: "req-1".to_string(),
            addr: "//tools/devenv:runner".to_string(),
            rewrite: rewrite(),
        };
        assert_eq!(PrepareRequest::decode(&req.encode()).expect("decode"), req);
    }

    /// The reason the payload is bytes and not strings. A program name, an
    /// argument or an environment value may be any byte sequence, and a lossy
    /// round trip would corrupt it silently rather than fail.
    #[test]
    fn non_utf8_values_survive() {
        let bad = OsString::from_vec(vec![0x66, 0x80, 0x6f]);
        let req = PrepareRequest {
            request_id: "r".to_string(),
            addr: "//a:b".to_string(),
            rewrite: SpecRewrite {
                program: PathBuf::from(bad.clone()),
                args: vec![bad.clone()],
                env: vec![(OsString::from("K"), bad.clone())],
                cwd: PathBuf::from("/"),
            },
        };
        let back = PrepareRequest::decode(&req.encode()).expect("decode");
        assert_eq!(back.rewrite.args[0], bad);
        assert_eq!(back.rewrite.env[0].1, bad);
        assert_eq!(back.rewrite.program, PathBuf::from(bad));
    }

    /// An empty value is a value, not a terminator — the length prefix is what
    /// makes that true.
    #[test]
    fn empty_values_are_preserved() {
        let req = PrepareRequest {
            request_id: String::new(),
            addr: String::new(),
            rewrite: SpecRewrite {
                program: PathBuf::new(),
                args: vec![OsString::new()],
                env: vec![(OsString::from("K"), OsString::new())],
                cwd: PathBuf::new(),
            },
        };
        assert_eq!(PrepareRequest::decode(&req.encode()).expect("decode"), req);
    }

    #[test]
    fn a_reply_round_trips_both_ways() {
        let ok = PrepareReply::Ok {
            rewrite: rewrite(),
            supplies_environment: true,
        };
        assert_eq!(PrepareReply::decode(&ok.encode()).expect("decode"), ok);
        let err = PrepareReply::Err("boom".to_string());
        assert_eq!(PrepareReply::decode(&err.encode()).expect("decode"), err);
    }

    /// A truncated frame is an error, not a panic or a silently short value:
    /// this decodes bytes that crossed a dylib boundary.
    #[test]
    fn a_truncated_frame_is_refused() {
        let req = PrepareRequest {
            request_id: "req".to_string(),
            addr: "//a:b".to_string(),
            rewrite: rewrite(),
        };
        let full = req.encode();
        for cut in [0, 1, 4, full.len() / 2, full.len() - 1] {
            assert!(
                PrepareRequest::decode(&full[..cut]).is_err(),
                "a {cut}-byte prefix must be refused"
            );
        }
    }
}
