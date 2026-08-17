//! macOS window bring-up.
//!
//! Floem creates the window *hidden* and only calls `set_visible(true)` after
//! wgpu has a device. Until that happens — and if the frame is restored onto a
//! display that is no longer plugged in — the process is running, Terminal
//! keeps the menu bar, and nothing appears on the monitor. A Dock click with
//! no visible windows used to do nothing.

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSScreen, NSWindow};
use objc2_foundation::{NSRect, NSSize};

use floem::AppEvent;

/// Pin wgpu to Metal before the event loop starts.
///
/// The default is "every backend". Probing Vulkan/GL on macOS can stall on the
/// UI thread, and the window stays invisible for the whole stall.
pub fn prefer_metal_backend() {
    if std::env::var_os("WGPU_BACKEND").is_some() {
        return;
    }
    // SAFETY: `main` calls this before any other thread exists.
    unsafe { std::env::set_var("WGPU_BACKEND", "metal") };
    eprintln!("Smithy: WGPU_BACKEND=metal");
}

pub fn handle_app_event(event: AppEvent) {
    match event {
        AppEvent::Reopen { .. } => {
            eprintln!("Smithy: Dock click — bringing windows forward");
            bring_windows_forward();
        }
        AppEvent::WillTerminate => {}
    }
}

/// Unhide, un-minimise, and move any window that sits off the current display.
pub fn bring_windows_forward() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);

    let screen = NSScreen::mainScreen(mtm);
    let mut n = 0usize;
    for window in app.windows() {
        n += 1;
        place_on_screen(&window, screen.as_deref());
        if window.isMiniaturized() {
            window.deminiaturize(None);
        }
        window.orderFrontRegardless();
        window.makeKeyAndOrderFront(None);
    }
    eprintln!("Smithy: {n} window(s) ordered front");
}

fn place_on_screen(window: &NSWindow, screen: Option<&NSScreen>) {
    let Some(screen) = screen else {
        return;
    };
    let visible = screen.visibleFrame();
    let frame = window.frame();
    if window.isVisible() && rects_intersect(frame, visible) {
        return;
    }
    window.setFrame_display(fit_on_screen(frame, visible), true);
}

fn rects_intersect(a: NSRect, b: NSRect) -> bool {
    let ax2 = a.origin.x + a.size.width;
    let ay2 = a.origin.y + a.size.height;
    let bx2 = b.origin.x + b.size.width;
    let by2 = b.origin.y + b.size.height;
    a.origin.x < bx2 && ax2 > b.origin.x && a.origin.y < by2 && ay2 > b.origin.y
}

fn fit_on_screen(frame: NSRect, visible: NSRect) -> NSRect {
    let width = frame.size.width.clamp(800.0, visible.size.width);
    let height = frame.size.height.clamp(600.0, visible.size.height);
    NSRect {
        origin: objc2_foundation::NSPoint {
            x: visible.origin.x + (visible.size.width - width) * 0.5,
            y: visible.origin.y + (visible.size.height - height) * 0.5,
        },
        size: NSSize { width, height },
    }
}
