//! The graph editor window.

mod geometry;
mod history;
mod patchbay;
mod theme;

use std::path::PathBuf;

use gpui::{
    App, AppContext, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, actions, px,
    size,
};
use tuyau_pipewire::Connection;

use crate::patchbay::Patchbay;

actions!(tuyau, [Quit, Undo, Redo]);

pub struct Options {
    pub config_path: PathBuf,
    pub state_path: PathBuf,
}

pub fn run(options: Options) -> anyhow::Result<()> {
    let connection = Connection::spawn()?;
    gpui_platform::application().run(move |cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("ctrl-z", Undo, None),
            KeyBinding::new("ctrl-shift-z", Redo, None),
            KeyBinding::new("ctrl-y", Redo, None),
        ]);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1280.0), px(820.0)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // Must match assets/tuyau.desktop, or launchers and docks show a
                // generic icon for the window.
                app_id: Some("tuyau".into()),
                titlebar: Some(TitlebarOptions {
                    title: Some("tuyau".into()),
                    ..TitlebarOptions::default()
                }),
                ..WindowOptions::default()
            },
            |window, cx| {
                let patchbay = cx.new(|cx| Patchbay::new(options, connection, cx));
                // Shortcuts reach the view through its focus.
                patchbay.read(cx).focus_handle().focus(window, cx);
                // The view is saved once, when leaving: on window close or on quit.
                let closing = patchbay.downgrade();
                window.on_window_should_close(cx, move |_, cx| {
                    let _ = closing.update(cx, |patchbay, _| patchbay.save_view());
                    true
                });
                patchbay
                    .update(cx, |_, cx| {
                        cx.on_app_quit(|patchbay, _| {
                            patchbay.save_view();
                            async {}
                        })
                    })
                    .detach();
                patchbay
            },
        );
        if let Err(error) = opened {
            log::error!("cannot open the window: {error}");
            cx.quit();
        }
        cx.activate(true);
    });
    Ok(())
}
