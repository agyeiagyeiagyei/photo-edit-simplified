// UI entry point: many signatures mirror the worker protocol and Leptos
// callback shapes, so folding them into structs would be churn without payoff.
#![allow(
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::missing_const_for_thread_local
)]

mod cache;
mod proxy;
mod ingest;
mod render;
mod export;
mod session_sync;
mod loupe;
mod drive_ui;
mod ui;
mod drive;
mod exif;
mod gpu;
mod zip;
mod perf;
mod pworker;
mod raw;
mod session;
mod prelude;
mod state;
mod web;

use prelude::*;

fn main() {
    console_error_panic_hook::set_once();
    perf::install();
    pworker::init();
    let _mount = perf::Span::new("app.mount");
    mount_to_body(|| view! { <App/> });
}
