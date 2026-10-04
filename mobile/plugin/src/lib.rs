//! What a webview cannot do with a file on a phone: open it in another app,
//! hand it to the share sheet, or copy it where the user keeps their files.
//! Android: `android/` (FileProvider, MediaStore). iOS: `ios/` (Quick Look,
//! UIActivityViewController, document picker). Desktop builds (`tauri dev` of the
//! mobile app) fall back to the system opener.

#[cfg(mobile)]
use serde::Serialize;
use tauri::plugin::{Builder, TauriPlugin};
use tauri::{Manager, Runtime};

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_pm_native);

#[cfg(mobile)]
#[derive(Serialize)]
struct PathArgs<'a> {
    path: &'a str,
}

pub struct Native<R: Runtime> {
    #[cfg(mobile)]
    handle: tauri::plugin::PluginHandle<R>,
    #[cfg(not(mobile))]
    _r: std::marker::PhantomData<fn() -> R>,
}

impl<R: Runtime> Native<R> {
    #[cfg(mobile)]
    fn call<T: serde::de::DeserializeOwned>(&self, cmd: &str, path: &str) -> Result<T, String> {
        self.handle.run_mobile_plugin(cmd, PathArgs { path }).map_err(|e| e.to_string())
    }

    /// Shows the file (preview on iOS, an app that can view it on Android).
    pub fn open(&self, path: &str) -> Result<(), String> {
        #[cfg(mobile)]
        return self.call("open", path);
        #[cfg(not(mobile))]
        return desktop_open(path);
    }

    /// The system share sheet.
    pub fn share(&self, path: &str) -> Result<(), String> {
        #[cfg(mobile)]
        return self.call("share", path);
        #[cfg(not(mobile))]
        return desktop_open(path);
    }

    /// Android: copies into Download/PopupMessenger and returns where it went.
    /// iOS: "Save to Files" picker (returns an empty string).
    pub fn save(&self, path: &str) -> Result<String, String> {
        #[cfg(mobile)]
        return self.call::<serde_json::Value>("save", path).map(|v| v["saved"].as_str().unwrap_or("").to_string());
        #[cfg(not(mobile))]
        return desktop_open(path).map(|_| String::new());
    }

    /// Android: reports the system bar and keyboard sizes to the page again
    /// (`window.__pmInsets`). Elsewhere CSS `env(safe-area-inset-*)` works.
    pub fn insets(&self) {
        #[cfg(target_os = "android")]
        let _ = self.call::<serde_json::Value>("insets", "");
    }

    /// A short tap of the vibration motor when a message arrives.
    pub fn haptic(&self) {
        #[cfg(mobile)]
        let _ = self.call::<serde_json::Value>("haptic", "");
    }

    /// iOS: asks for (`true`) or gives back (`false`) a little time to keep running
    /// after the app leaves the screen, so the relay connection can sign off instead
    /// of being frozen open. Android keeps running on its own for that long.
    pub fn keep_running(&self, on: bool) {
        #[cfg(target_os = "ios")]
        let _ = self.handle.run_mobile_plugin::<serde_json::Value>("keepRunning", serde_json::json!({ "on": on }));
        #[cfg(not(target_os = "ios"))]
        let _ = on;
    }
}

#[cfg(not(mobile))]
fn desktop_open(path: &str) -> Result<(), String> {
    #[cfg(windows)]
    let r = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(path).spawn();
    #[cfg(not(any(windows, target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(path).spawn();
    r.map(|_| ()).map_err(|e| e.to_string())
}

pub trait NativeExt<R: Runtime> {
    fn native(&self) -> &Native<R>;
}

impl<R: Runtime, T: Manager<R>> NativeExt<R> for T {
    fn native(&self) -> &Native<R> {
        self.state::<Native<R>>().inner()
    }
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("pm-native")
        .setup(|app, _api| {
            #[cfg(target_os = "android")]
            let handle = _api.register_android_plugin("com.popupmessenger.pmnative", "NativePlugin")?;
            #[cfg(target_os = "ios")]
            let handle = _api.register_ios_plugin(init_plugin_pm_native)?;
            app.manage(Native::<R> {
                #[cfg(mobile)]
                handle,
                #[cfg(not(mobile))]
                _r: std::marker::PhantomData,
            });
            Ok(())
        })
        .build()
}
