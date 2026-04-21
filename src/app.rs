use crate::autostart;
use crate::config::{self, SharedConfig};
use crate::install;
use crate::monitor;
use crate::server;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIconBuilder, TrayIconEvent};

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

fn build_icon() -> tray_icon::Icon {
    let bytes = include_bytes!("../tray-icon.png");
    let img = image::load_from_memory(bytes)
        .expect("decode tray-icon.png")
        .to_rgba8();
    let (w, h) = img.dimensions();
    tray_icon::Icon::from_rgba(img.into_raw(), w, h).expect("icon build")
}

fn open_url(url: &str) {
    unsafe {
        let op = HSTRING::from("open");
        let file = HSTRING::from(url);
        ShellExecuteW(
            HWND(std::ptr::null_mut()),
            PCWSTR(op.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn panel_url(addr: &SocketAddr) -> String {
    format!("http://127.0.0.1:{}/", addr.port())
}

pub fn run() {
    if install::install_if_needed() {
        return;
    }
    autostart::ensure_enabled();

    let cfg: SharedConfig = config::load();
    let stop = Arc::new(AtomicBool::new(false));

    let server_state = server::start(cfg.clone(), stop.clone());
    monitor::spawn(cfg.clone(), stop.clone());

    let need_setup = !cfg.read().unwrap().setup_complete;
    if need_setup {
        open_url(&panel_url(&server_state.addr));
    }

    let event_loop = EventLoopBuilder::new().build();

    let menu = Menu::new();
    let open_item = MenuItem::new("Open control panel", true, None);
    let status_item = MenuItem::new(
        format!("Listening on 127.0.0.1:{}", server_state.addr.port()),
        false,
        None,
    );
    menu.append(&open_item).ok();
    menu.append(&status_item).ok();
    menu.append(&PredefinedMenuItem::separator()).ok();
    let about = MenuItem::new("Quit requires 2FA — open panel", false, None);
    menu.append(&about).ok();

    let _tray = TrayIconBuilder::new()
        .with_tooltip("BetWall")
        .with_icon(build_icon())
        .with_menu(Box::new(menu))
        .build()
        .expect("tray build");

    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayIconEvent::receiver();
    let open_id = open_item.id().0.clone();
    let panel = panel_url(&server_state.addr);
    let stop_flag = stop.clone();

    event_loop.run(move |_event, _target, control_flow| {
        *control_flow = ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(100),
        );

        if stop_flag.load(Ordering::Relaxed) {
            *control_flow = ControlFlow::Exit;
            return;
        }

        while let Ok(event) = menu_channel.try_recv() {
            if event.id.0 == open_id {
                open_url(&panel);
            }
        }

        while let Ok(event) = tray_channel.try_recv() {
            if let TrayIconEvent::DoubleClick { .. } = event {
                open_url(&panel);
            }
        }
    });
}
