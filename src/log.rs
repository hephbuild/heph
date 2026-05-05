use slog::{Drain, Logger, o};
use slog_term::{CompactFormat, TermDecorator};

pub fn init() -> Logger {
    let decorator = TermDecorator::new().stdout().build();
    let drain = CompactFormat::new(decorator)
        .use_custom_timestamp(|_: &mut dyn std::io::Write| Ok(()))
        .build()
        .fuse();

    let drain = slog_async::Async::new(drain).build().fuse();

    let logger = Logger::root(drain, o!());

    // Set slog as global log handler; error means it was already initialized, which is fine
    drop(slog_stdlog::init());

    logger
}
