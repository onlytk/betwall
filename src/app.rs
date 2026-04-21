use crate::autostart;
use crate::config::{self, SharedConfig};
use crate::install;
use crate::monitor;
use crate::server;
use crate::updater;
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
    const SIZE: u32 = 64;
    const RED: [u8; 3] = [201, 44, 44];
    let center = (SIZE as f32 - 1.0) / 2.0;
    let outer_r: f32 = 26.0;
    let inner_r: f32 = 20.0;
    let bar_hw: f32 = 4.0;
    let bar_hh: f32 = 24.0;
    let (sin, cos) = (-std::f32::consts::FRAC_PI_4).sin_cos();

    let mut px = vec![0u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let d = (dx * dx + dy * dy).sqrt();
            let rx = dx * cos - dy * sin;
            let ry = dx * sin + dy * cos;

            let ring_sd = (d - outer_r).max(inner_r - d);
            let bar_sd = (rx.abs() - bar_hw)
                .max(ry.abs() - bar_hh)
                .max(d - outer_r);

            let ring_cov = (0.5 - ring_sd).clamp(0.0, 1.0);
            let bar_cov = (0.5 - bar_sd).clamp(0.0, 1.0);
            let alpha = (ring_cov.max(bar_cov) * 255.0) as u8;
            if alpha > 0 {
                let i = ((y * SIZE + x) * 4) as usize;
                px[i] = RED[0];
                px[i + 1] = RED[1];
                px[i + 2] = RED[2];
                px[i + 3] = alpha;
            }
        }
    }
    tray_icon::Icon::from_rgba(px, SIZE, SIZE).expect("icon build")
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

    let update_status = updater::shared();
    updater::spawn_checker(update_status.clone(), stop.clone());

    let server_state = server::start(cfg.clone(), stop.clone(), update_status.clone());
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
