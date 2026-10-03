use crate::state::{AppState, Status};
use crate::{net, ui};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, Wry};

const TRAY: &str = "main";
/// Recipients shown directly in the tray menu; the rest go behind "더보기".
const TOP_N: usize = 10;

/// Placeholder icon drawn in code until real icons exist: a dot whose color
/// always reflects the status, plus a small red badge while there is unread mail.
/// (Unread used to recolor the whole dot orange, which made a switch from DND
/// back to online look like the wrong status.)
fn icon(status: Status, unread: bool) -> Image<'static> {
    const S: u32 = 32;
    let base = match status {
        Status::Online => [74, 144, 217],
        Status::Dnd => [142, 68, 173],
        Status::Offline => [140, 140, 140],
    };
    let badge = unread;
    // Coverage of a disc of radius r centered at (cx, cy), anti-aliased over one pixel.
    let disc = |x: f32, y: f32, cx: f32, cy: f32, r: f32| (r - ((x - cx).powi(2) + (y - cy).powi(2)).sqrt()).clamp(0.0, 1.0);
    let mut px = Vec::with_capacity((S * S * 4) as usize);
    for y in 0..S {
        for x in 0..S {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let mut c = [base[0], base[1], base[2], (disc(fx, fy, 16.0, 16.0, 13.5) * 255.0) as u8];
            if badge {
                let ring = disc(fx, fy, 25.0, 7.0, 7.5); // transparent gap around the badge
                let dot = disc(fx, fy, 25.0, 7.0, 6.0);
                if dot > 0.0 {
                    c = [229, 57, 53, (dot * 255.0) as u8];
                } else if ring > 0.0 {
                    c[3] = (c[3] as f32 * (1.0 - ring)) as u8;
                }
            }
            px.extend_from_slice(&c);
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
    let mut peers = st.peers(true);
    // A user with a relay account is one entry; their devices are in the picker.
    let users: std::collections::HashSet<String> = peers.iter().filter(|p| p.relay).map(|p| p.account.clone()).collect();
    peers.retain(|p| p.relay || p.account.is_empty() || !users.contains(&p.account));
    // Groups with nobody online are left out to keep the menu short.
    let groups: Vec<_> = st.groups().into_iter().filter(|g| g.online > 0).collect();
    let unread = st.unread();
    let (notify, status) = {
        let s = st.settings.lock().unwrap();
        (s.notify, s.status)
    };

    let m = Menu::new(app)?;
    let item = |s: Status| CheckMenuItem::with_id(app, format!("st:{}", s.label()), s.label(), true, status == s, None::<&str>);
    let (on, dnd, off) = (item(Status::Online)?, item(Status::Dnd)?, item(Status::Offline)?);
    m.append(&Submenu::with_items(app, format!("상태: {}", status.label()), true, &[&on, &dnd, &off])?)?;
    m.append(&PredefinedMenuItem::separator(app)?)?;
    for g in &groups {
        let text = format!("[그룹] {}  ({}/{}명)", g.name, g.online, g.members.len());
        m.append(&MenuItem::with_id(app, format!("g:{}", g.id), label(&text), true, None::<&str>)?)?;
    }
    m.append(&MenuItem::with_id(app, "groups", "전송 그룹 관리…", true, None::<&str>)?)?;
    m.append(&PredefinedMenuItem::separator(app)?)?;
    if peers.is_empty() {
        m.append(&MenuItem::with_id(app, "none", "(접속 중인 사용자 없음)", false, None::<&str>)?)?;
    }
    for p in peers.iter().take(TOP_N) {
        let mut text = if p.host.is_empty() { p.display.clone() } else { format!("{}  ({})", p.display, p.host) };
        if p.status == Status::Dnd {
            text.push_str("  — 방해금지");
        }
        m.append(&MenuItem::with_id(app, format!("p:{}", p.id), label(&text), true, None::<&str>)?)?;
    }
    if peers.len() > TOP_N {
        m.append(&MenuItem::with_id(app, "more", format!("더보기… (전체 {}명)", peers.len()), true, None::<&str>)?)?;
    }
    m.append(&PredefinedMenuItem::separator(app)?)?;
    m.append(&MenuItem::with_id(app, "search", "연락처 검색…", true, None::<&str>)?)?;
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
        .icon(icon(Status::Online, false))
        .tooltip("PopupMessenger")
        .menu(&build_menu(app)?)
        .show_menu_on_left_click(true) // left and right click open the same menu
        .on_menu_event(|app, e| on_menu(app, e.id().as_ref()))
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
        let st = a.state::<AppState>();
        let unread = st.unread();
        let status = st.settings.lock().unwrap().status;
        let _ = tray.set_icon(Some(icon(status, unread > 0)));
        let mut tip = format!("PopupMessenger - {}", status.label());
        if unread > 0 {
            tip.push_str(&format!(" · 안 읽은 쪽지 {unread}개"));
        }
        let _ = tray.set_tooltip(Some(tip));
    });
}

fn on_menu(app: &AppHandle, id: &str) {
    let st = app.state::<AppState>();
    match id {
        "inbox" => ui::open_inbox(app),
        "more" => ui::open_picker(app, vec![]),
        "groups" => ui::open_groups(app),
        "search" => ui::open_search(app),
        "settings" => ui::open_settings(app),
        "scan" => {
            st.scan.notify_one();
            crate::relay::refresh(app);
        }
        "notify" => {
            {
                let mut s = st.settings.lock().unwrap();
                s.notify = !s.notify;
            }
            st.save_settings();
            st.scan.notify_one(); // re-announce the new notify flag
            crate::relay::announce(app);
            refresh(app);
        }
        "quit" => {
            let a = app.clone();
            tauri::async_runtime::spawn(async move {
                net::send_bye(&a).await;
                crate::relay::logout(&a).await;
                a.exit(0);
            });
        }
        s if s.starts_with("st:") => {
            let status = [Status::Online, Status::Dnd, Status::Offline].into_iter().find(|x| s[3..] == *x.label());
            if let Some(status) = status {
                st.settings.lock().unwrap().status = status;
                st.save_settings();
                st.scan.notify_one(); // announce the new status right away
                crate::relay::announce(app);
                refresh(app);
            }
        }
        s if s.starts_with("g:") => ui::open_group_compose(app, &s[2..]),
        s if s.starts_with("p:") => ui::open_compose(app, vec![s[2..].to_string()], vec![], None),
        _ => {}
    }
}
