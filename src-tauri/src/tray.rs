use crate::state::AppState;
use crate::{net, ui};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

const TRAY: &str = "main";
/// Recipients shown directly in the tray menu; the rest go behind "더보기".
const TOP_N: usize = 10;

/// Placeholder icon (plain dot) drawn in code until real icons exist.
fn icon(unread: bool) -> Image<'static> {
    const S: u32 = 32;
    let (r, g, b) = if unread { (232, 113, 10) } else { (74, 144, 217) };
    let mut px = Vec::with_capacity((S * S * 4) as usize);
    for y in 0..S {
        for x in 0..S {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let a = ((13.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0) * 255.0) as u8;
            px.extend_from_slice(&[r, g, b, a]);
        }
    }
    Image::new_owned(px, S, S)
}

fn label(s: &str) -> String {
    // '&' marks a mnemonic in Windows menus.
    if cfg!(windows) { s.replace('&', "&&") } else { s.to_string() }
}

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let st = app.state::<AppState>();
    let peers = st.peers(true);
    let unread = st.unread();
    let notify = st.settings.lock().unwrap().notify;

    let m = Menu::new(app)?;
    if peers.is_empty() {
        m.append(&MenuItem::with_id(app, "none", "(접속 중인 사용자 없음)", false, None::<&str>)?)?;
    }
    for p in peers.iter().take(TOP_N) {
        let text = if p.host.is_empty() { p.name.clone() } else { format!("{}  ({})", p.name, p.host) };
        m.append(&MenuItem::with_id(app, format!("p:{}", p.id), label(&text), true, None::<&str>)?)?;
    }
    if peers.len() > TOP_N {
        m.append(&MenuItem::with_id(app, "more", format!("더보기… (전체 {}명)", peers.len()), true, None::<&str>)?)?;
    }
    m.append(&PredefinedMenuItem::separator(app)?)?;
    let inbox = if unread > 0 { format!("받은 쪽지 ({unread})") } else { "받은 쪽지".into() };
    m.append(&MenuItem::with_id(app, "inbox", inbox, true, None::<&str>)?)?;
    m.append(&CheckMenuItem::with_id(app, "notify", "알림 팝업", true, notify, None::<&str>)?)?;
    m.append(&MenuItem::with_id(app, "scan", "다시 탐색", true, None::<&str>)?)?;
    m.append(&MenuItem::with_id(app, "settings", "설정…", true, None::<&str>)?)?;
    m.append(&PredefinedMenuItem::separator(app)?)?;
    m.append(&MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?)?;
    Ok(m)
}

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    TrayIconBuilder::with_id(TRAY)
        .icon(icon(false))
        .tooltip("PopupMessenger")
        .menu(&build_menu(app)?)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, e| on_menu(app, e.id().as_ref()))
        .on_tray_icon_event(|tray, e| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                ui::open_inbox(tray.app_handle());
            }
        })
        .build(app)?;
    refresh(app);
    Ok(())
}

/// Rebuilds the menu and swaps icon/tooltip for the unread state.
pub fn refresh(app: &AppHandle) {
    let a = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(tray) = a.tray_by_id(TRAY) else { return };
        if let Ok(m) = build_menu(&a) {
            let _ = tray.set_menu(Some(m));
        }
        let unread = a.state::<AppState>().unread();
        let _ = tray.set_icon(Some(icon(unread > 0)));
        let tip = if unread > 0 { format!("PopupMessenger - 안 읽은 쪽지 {unread}개") } else { "PopupMessenger".into() };
        let _ = tray.set_tooltip(Some(tip));
    });
}

fn on_menu(app: &AppHandle, id: &str) {
    let st = app.state::<AppState>();
    match id {
        "inbox" => ui::open_inbox(app),
        "more" => ui::open_picker(app, vec![]),
        "settings" => ui::open_settings(app),
        "scan" => st.scan.notify_one(),
        "notify" => {
            {
                let mut s = st.settings.lock().unwrap();
                s.notify = !s.notify;
            }
            st.save_settings();
            st.scan.notify_one(); // re-announce the new notify flag
            refresh(app);
        }
        "quit" => {
            let a = app.clone();
            tauri::async_runtime::spawn(async move {
                net::send_bye(&a).await;
                a.exit(0);
            });
        }
        s if s.starts_with("p:") => ui::open_compose(app, vec![s[2..].to_string()], vec![], None),
        _ => {}
    }
}
