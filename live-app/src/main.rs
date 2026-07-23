//! Minimal wry webview host for Nekomaru LiveUI.

use nkcore::prelude::*;
use nkcore::winit::{
    AppEvent,
    EventLoopExt as _,
};

use clap::Parser;

use winit::{
    dpi::LogicalSize,
    dpi::PhysicalPosition,
    dpi::Size as WindowSize,
    event_loop::EventLoop,
    event::WindowEvent,
    window::Window,
    window::WindowButtons,
};

/// CLI arguments for the webview host.
#[derive(Parser)]
#[command(name = "live-app", about = "Minimal wry webview host for Nekomaru LiveUI")]
struct Args {
    /// URL to load in the webview.
    pub url: String,

    /// Window title to use for the webview.
    #[arg(long, short = 't')]
    pub title: String,

    /// Initial width of the window client area in logical pixels. Must be
    /// provided if `height` is provided.
    ///
    /// Logical pixels are converted to physical pixels using the provided
    /// `scale_factor` or the system scaling factor if `scale_factor` is not
    /// provided.
    #[arg(long, short = 'x', requires = "height", default_value = "1280")]
    pub width: u32,

    /// Initial height of the window client area in logical pixels. Must be
    /// provided if `width` is provided.
    ///
    /// Logical pixels are converted to physical pixels using the provided
    /// `scale_factor` or the system scaling factor if `scale_factor` is not
    /// provided.
    #[arg(long, short = 'y', requires = "width", default_value = "720")]
    pub height: u32,

    /// Device scaling factor to use for the webview. If not provided, the
    /// webview will use the system scaling factor, which may cause certain
    /// issues on high-DPI displays.
    #[arg(long, short = 's')]
    pub scale_factor: Option<f32>,

    /// Allow resizing the host window for frontend layout testing. Production
    /// windows remain fixed-size unless this flag is provided.
    #[arg(long)]
    pub resizable: bool,
}

fn main() {
    pretty_env_logger::init();

    let Args { url, title, width, height, scale_factor, resizable } = Args::parse();
    log::info!(
        "loading app at {url}, scale factor: {scale_factor:?}, resizable: {resizable}");

    let window_size =
        LogicalSize::new(width, height)
            .pipe(|logical_size| -> WindowSize {
                if let Some(scale_factor) = scale_factor {
                    logical_size.to_physical::<f64>(scale_factor as f64).into()
                } else {
                    logical_size.into()
                }
            });


    let mut webview_args = vec![
        // Disable WebView2's background throttling features to prevent the webview
        // from freezing when the window is not in the foreground. This is necessary
        // for streaming.
        Borrowed("--disable-backgrounding-occluded-windows"),
        Borrowed("--disable-renderer-backgrounding"),
    ];

    if let Some(scale_factor) = scale_factor {
        webview_args.push(Owned(format!("--force-device-scale-factor={scale_factor}")));
    }

    // SAFETY: Single-threaded access to environment variable, set before
    // any threads are spawned.
    unsafe {
        std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", webview_args.join(" "));
    }

    EventLoop::<()>::new()
        .expect("failed to create event loop")
        .run_app_with(move |event_loop| {
            let window =
                event_loop.create_window(
                    Window::default_attributes()
                        .with_title(&title)
                        .with_inner_size(window_size)
                        .with_resizable(resizable)
                        .with_enabled_buttons(
                            WindowButtons::CLOSE |
                            WindowButtons::MINIMIZE))
                    .expect("failed to create window");

            let webview =
                wry::WebViewBuilder::new()
                    .with_url(&url)
                    .with_devtools(true)
                    .build(&window)
                    .expect("failed to create webview");

            move |event_loop, event| {
                if let AppEvent::WindowEvent(window_id, event) = event &&
                    window_id == window.id() {
                    match event {
                        WindowEvent::CloseRequested =>
                            event_loop.exit(),
                        WindowEvent::Resized(new_size) => {
                            let _ = webview.set_bounds(wry::Rect {
                                position: PhysicalPosition::new(0, 0).into(),
                                size: new_size.into(),
                            });
                        }
                        _ => {}
                    }
                }
            }
        })
        .expect("failed to run event loop");
}
