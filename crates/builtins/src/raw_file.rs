//! A target whose one output is a single file whose bytes the driver holds in
//! memory: the `textfile` and `template` drivers.

use hplugin::driver::{RunResponse, outputartifact};
use xxhash_rust::xxh3::Xxh3;

/// `out` under package `pkg`: `<pkg>/<out>`, or `out` itself in the root
/// package.
pub(crate) fn pkg_out(pkg: &str, out: String) -> String {
    if pkg.is_empty() {
        out
    } else {
        format!("{pkg}/{out}")
    }
}

/// The run's one artifact: `data` as a raw file at `out`, named after `out`'s
/// file name.
///
/// Its hashout is xxh3 over `data`, then `out`, then the executable bit as one
/// byte, in unpadded lowercase hex. It reaches cache keys, so that byte stream
/// must not change.
pub(crate) fn raw_file_response(data: Vec<u8>, out: &str, executable: bool) -> RunResponse {
    let mut h = Xxh3::new();
    h.update(&data);
    h.update(out.as_bytes());
    h.update(&[executable as u8]);
    let hashout = format!("{:x}", h.digest());

    let name = std::path::Path::new(out)
        .file_name()
        .map_or_else(|| out.to_string(), |s| s.to_string_lossy().into_owned());

    RunResponse {
        artifacts: vec![outputartifact::OutputArtifact {
            group: String::new(),
            name,
            r#type: outputartifact::Type::Output,
            content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                data,
                path: out.to_string(),
                x: executable,
            }),
            hashout,
        }],
        ..Default::default()
    }
}
