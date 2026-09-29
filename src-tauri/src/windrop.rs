//! Windows file drops.
//!
//! wry's own drop handler registers an OLE drop target on WebView2's child
//! window and turns WebView2's external drops off, but only after navigation
//! has started. If Chromium creates its render window first, Chromium's own
//! (now refusing) drop target sits in front of wry's and every drop shows the
//! "not allowed" cursor. That race cannot be fixed from outside, so on Windows
//! the wry handler is disabled (see `ui.rs`) and WebView2 takes the drop
//! itself: the page forwards the dropped `File` objects with
//! `chrome.webview.postMessageWithAdditionalObjects("pm-drop", files)`, and
//! this handler reads their real paths and hands them to `window.__pmDrop`.

use tauri::WebviewWindow;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2File, ICoreWebView2WebMessageReceivedEventArgs, ICoreWebView2WebMessageReceivedEventArgs2,
};
use webview2_com::{take_pwstr, WebMessageReceivedEventHandler};
use windows_core::{Interface, PWSTR};

const MESSAGE: &str = "pm-drop";

pub fn attach(w: &WebviewWindow) {
    let target = w.clone();
    let _ = w.with_webview(move |pw| unsafe {
        let Ok(core) = pw.controller().CoreWebView2() else { return };
        let target = target.clone();
        let handler = WebMessageReceivedEventHandler::create(Box::new(move |_, args| {
            if let Some(paths) = args.and_then(|a| dropped_paths(&a)) {
                let js = format!("window.__pmDrop && window.__pmDrop({})", serde_json::to_string(&paths).unwrap_or_default());
                let _ = target.eval(&js);
            }
            Ok(())
        }));
        let mut token = 0i64;
        let _ = core.add_WebMessageReceived(&handler, &mut token);
    });
}

/// Paths of the files/folders attached to a "pm-drop" message; `None` for any other message
/// (Tauri's own IPC may use web messages too).
unsafe fn dropped_paths(args: &ICoreWebView2WebMessageReceivedEventArgs) -> Option<Vec<String>> {
    let mut msg = PWSTR::null();
    args.TryGetWebMessageAsString(&mut msg).ok()?;
    if take_pwstr(msg) != MESSAGE {
        return None;
    }
    let objects = args.cast::<ICoreWebView2WebMessageReceivedEventArgs2>().ok()?.AdditionalObjects().ok()?;
    let mut count = 0u32;
    objects.Count(&mut count).ok()?;
    let mut paths = Vec::new();
    for i in 0..count {
        let Ok(file) = objects.GetValueAtIndex(i).and_then(|o| o.cast::<ICoreWebView2File>()) else { continue };
        let mut path = PWSTR::null();
        if file.Path(&mut path).is_ok() {
            paths.push(take_pwstr(path));
        }
    }
    Some(paths)
}
