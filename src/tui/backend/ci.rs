use crate::tui::app::{App, AppContext};
use crate::tui::log_sink::LogSink;

pub async fn run<A: App>(app: A, sink: LogSink) -> anyhow::Result<A::Output> {
    tracing::info!("{}", app.label());
    let ctx = AppContext::direct(sink);
    app.run(ctx).await
}
