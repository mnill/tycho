mod integration;

pub fn try_init_test_tracing() {
    use std::io::IsTerminal;
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .with_writer(std::io::stdout)
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with_file(false)
        .with_line_number(false)
        .with_target(false)
        .with_thread_ids(true)
        .compact()
        .try_init()
        .ok();
}
