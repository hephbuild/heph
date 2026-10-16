use anyhow::Context;
use clap::{Args, Subcommand};

use crate::commands::bootstrap;
use crate::telemetry::{DisabledBy, FlushMode, Status};

#[derive(Args)]
pub struct TelemetryArgs {
    #[command(subcommand)]
    pub command: TelemetryCommands,
}

#[derive(Subcommand)]
pub enum TelemetryCommands {
    /// Show whether telemetry is on, why, and what is waiting to be sent
    ///
    /// Reports whether this machine and workspace send anonymous usage
    /// telemetry, which switch turned it off (`HEPH_DISABLE_TELEMETRY`, or
    /// `telemetry.enabled: false` in the config), where the spool of unsent
    /// events lives, how many are pending and how old the oldest is, how the
    /// last flush went, and when the next one is due. It reports and sends
    /// nothing itself (this command is recorded like any other, after the
    /// report is printed). `--json` emits the same as a stable object (see
    /// docs/TELEMETRY.md for its keys).
    ///
    /// Example: `heph tool telemetry status --json`
    Status(StatusArgs),
}

#[derive(Args, Clone)]
pub struct StatusArgs {
    /// Emit JSON instead of the text report
    #[arg(long)]
    pub json: bool,
}

impl TelemetryArgs {
    pub fn execute(&self) -> anyhow::Result<()> {
        match &self.command {
            TelemetryCommands::Status(args) => status(args),
        }
    }
}

fn status(args: &StatusArgs) -> anyhow::Result<()> {
    let status = crate::telemetry::status(bootstrap::telemetry_enabled_from_config())
        .context("reading the telemetry status")?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&status).context("encoding the telemetry status")?
        );
    } else {
        for line in render_text(&status) {
            println!("{line}");
        }
    }
    Ok(())
}

/// A duration in ms as a short human age: `42s`, `3m`, `2h`, `5d`.
fn human_ms(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// The text report: one `key: value` line per fact.
fn render_text(s: &Status) -> Vec<String> {
    let mut lines = Vec::new();
    let state = if s.enabled {
        match s.flush_mode {
            FlushMode::Synchronous => {
                "enabled (CI: sent synchronously by every command)".to_string()
            }
            FlushMode::Detached | FlushMode::Off => "enabled".to_string(),
        }
    } else {
        let why: Vec<&str> = s
            .disabled_by
            .iter()
            .map(|d| match d {
                DisabledBy::Env => "HEPH_DISABLE_TELEMETRY is set",
                DisabledBy::Config => "telemetry.enabled is false in the config",
            })
            .collect();
        format!("disabled ({})", why.join("; "))
    };
    lines.push(format!("telemetry:   {state}"));

    let mut endpoint = s.endpoint.clone();
    if s.endpoint_overridden {
        endpoint.push_str(&format!("  (from {})", crate::telemetry::ENDPOINT_ENV));
    }
    lines.push(format!("endpoint:    {endpoint}"));
    if let Some(e) = &s.endpoint_error {
        lines.push(format!("             rejected, nothing is sent: {e}"));
    }

    let spool = s
        .spool_path
        .as_deref()
        .unwrap_or("(no HOME or XDG_CONFIG_HOME)");
    lines.push(format!("spool:       {spool}"));
    let pending = match s.oldest_event_age_ms {
        Some(age) if s.pending_events > 0 => {
            format!(
                "{} event(s), oldest {} old",
                s.pending_events,
                human_ms(age)
            )
        }
        _ => "none".to_string(),
    };
    lines.push(format!("pending:     {pending}"));

    let last = match (s.last_flush_ms, &s.last_flush_error) {
        (None, _) => "never".to_string(),
        (Some(at), None) => format!("{} ago, ok", human_ms(s.now_ms - at)),
        (Some(at), Some(e)) => format!("{} ago, failed: {e}", human_ms(s.now_ms - at)),
    };
    lines.push(format!("last flush:  {last}"));

    let next = match (s.flush_mode, s.next_flush_due_ms) {
        (FlushMode::Off, _) => "never (disabled)".to_string(),
        (FlushMode::Synchronous, _) => "at the end of every command (CI)".to_string(),
        (FlushMode::Detached, None) => "nothing pending".to_string(),
        (FlushMode::Detached, Some(due)) if due <= s.now_ms => {
            "due: the next command starts one".to_string()
        }
        (FlushMode::Detached, Some(due)) => {
            format!("in {} (or at 50 pending events)", human_ms(due - s.now_ms))
        }
    };
    lines.push(format!("next flush:  {next}"));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> Status {
        Status {
            enabled: true,
            disabled_by: vec![],
            ci: false,
            flush_mode: FlushMode::Detached,
            endpoint: "https://eu.posthog.com".to_string(),
            endpoint_overridden: false,
            endpoint_error: None,
            spool_path: Some("/home/u/.config/heph/telemetry-spool.db".to_string()),
            pending_events: 3,
            oldest_event_ms: Some(1_000),
            oldest_event_age_ms: Some(120_000),
            last_flush_ms: Some(1_000),
            last_flush_ok: Some(false),
            last_flush_error: Some("connection refused".to_string()),
            last_flusher_spawn_ms: Some(1_000),
            next_flush_due_ms: Some(181_000),
            now_ms: 121_000,
        }
    }

    #[test]
    fn text_report_names_each_fact() {
        let lines = render_text(&status());
        assert_eq!(
            lines,
            [
                "telemetry:   enabled",
                "endpoint:    https://eu.posthog.com",
                "spool:       /home/u/.config/heph/telemetry-spool.db",
                "pending:     3 event(s), oldest 2m old",
                "last flush:  2m ago, failed: connection refused",
                "next flush:  in 1m (or at 50 pending events)",
            ]
        );
    }

    #[test]
    fn text_report_says_why_it_is_disabled() {
        let s = Status {
            enabled: false,
            disabled_by: vec![DisabledBy::Env, DisabledBy::Config],
            flush_mode: FlushMode::Off,
            pending_events: 0,
            oldest_event_ms: None,
            oldest_event_age_ms: None,
            last_flush_ms: None,
            last_flush_ok: None,
            last_flush_error: None,
            next_flush_due_ms: None,
            ..status()
        };
        let lines = render_text(&s);
        assert_eq!(
            lines.first().map(String::as_str),
            Some(
                "telemetry:   disabled (HEPH_DISABLE_TELEMETRY is set; \
                 telemetry.enabled is false in the config)"
            )
        );
        assert!(lines.contains(&"pending:     none".to_string()));
        assert!(lines.contains(&"last flush:  never".to_string()));
        assert!(lines.contains(&"next flush:  never (disabled)".to_string()));
    }

    #[test]
    fn human_ages() {
        assert_eq!(human_ms(-5), "0s");
        assert_eq!(human_ms(59_999), "59s");
        assert_eq!(human_ms(60_000), "1m");
        assert_eq!(human_ms(3_600_000), "1h");
        assert_eq!(human_ms(86_400_000 * 3), "3d");
    }
}
