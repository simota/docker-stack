// Hide the console window on Windows release builds; harmless elsewhere.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod backend;
mod commands;
mod config;
mod dto;
mod exits;
mod files;
mod logging;
mod run_spec;
mod state;
mod streams;
mod ui;

fn main() {
    logging::init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("backend")
        .build()
        .expect("failed to start the tokio runtime");
    engine::set_runtime_handle(runtime.handle().clone());
    // The UI thread spawns backend tasks directly.
    let _enter = runtime.enter();

    let config = config::Config::load();
    let target = config.initial_target();
    let (backend, events) = backend::Backend::new(
        state::AppState::new(target),
        config::ConfigStore::new(config),
    );
    state::connect(&backend, None);
    state::start_daemon_if_down(&backend);
    let app = backend.clone();
    backend.spawn(async move {
        let available = engine::compose_available().await;
        state::set_compose_available(&app, available);
    });
    ui::run(backend, events);
}
