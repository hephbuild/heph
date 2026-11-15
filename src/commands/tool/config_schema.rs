use std::io::Write;

use anyhow::Context;

use crate::engine::config_yaml;

#[derive(clap::Args)]
pub struct Args {}

/// Print the `.hephconfig` JSON Schema to stdout.
///
/// Reads no config and starts no engine: the schema is this binary's, so a
/// workspace that pins a version gets the pinned binary's (the self-upgrade
/// has already re-executed into it by the time this runs).
pub fn execute(_args: &Args) -> anyhow::Result<()> {
    let text = config_yaml::config_json_schema_text().context("rendering the config schema")?;
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())
        .context("writing the config schema to stdout")?;
    out.flush().context("flushing stdout")?;
    Ok(())
}
