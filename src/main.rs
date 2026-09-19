use dioxus::prelude::*;
use std::sync::{Arc, Mutex};

use myphin::store::Store;

mod ui;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("myphin=info")),
        )
        .init();

    // cargo run launches a raw binary, not a .app. Promote it so macOS
    // treats it as a regular GUI app (Dock, menu bar, Cmd+Q).
    #[cfg(target_os = "macos")]
    promote_to_foreground_app();
    #[cfg(target_os = "macos")]
    set_dock_icon();

    // Keep Dioxus's default native menu. Edit enables paste; Quit is Cmd+Q.
    LaunchBuilder::desktop()
        .with_cfg(
            dioxus::desktop::Config::new().with_window(
                dioxus::desktop::WindowBuilder::new()
                    .with_title("Myphin")
                    .with_inner_size(dioxus::desktop::LogicalSize::new(1120.0, 780.0)),
            ),
        )
        .launch(ui::App);
}

/// Turn a command-line process into a foreground app. Without this, `cargo run`
/// stays attached to Terminal: no app menu, so paste and Cmd+Q do not work.
#[cfg(target_os = "macos")]
fn promote_to_foreground_app() {
    #[repr(C)]
    struct ProcessSerialNumber {
        high: u32,
        low: u32,
    }

    const K_CURRENT_PROCESS: u32 = 2;
    const K_PROCESS_TRANSFORM_TO_FOREGROUND_APPLICATION: u32 = 1;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn TransformProcessType(psn: *mut ProcessSerialNumber, transform_state: u32) -> i32;
    }

    let mut psn = ProcessSerialNumber {
        high: 0,
        low: K_CURRENT_PROCESS,
    };
    unsafe {
        TransformProcessType(&mut psn, K_PROCESS_TRANSFORM_TO_FOREGROUND_APPLICATION);
    }
}

/// Give the Dock and Cmd+Tab switcher the app icon. A bundled .app gets this
/// from Info.plist, but `cargo run` has no bundle, so set it on the process.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::AllocAnyThread;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{MainThreadMarker, NSData};

    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!("dock icon skipped: not on the main thread");
        return;
    };
    let data = NSData::with_bytes(include_bytes!("../assets/icon.png"));
    match NSImage::initWithData(NSImage::alloc(), &data) {
        // Safety: called on the main thread with a valid, decoded NSImage.
        Some(image) => unsafe {
            NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image))
        },
        None => tracing::warn!("dock icon skipped: assets/icon.png did not decode"),
    }
}

pub type SharedStore = Arc<Mutex<Store>>;
