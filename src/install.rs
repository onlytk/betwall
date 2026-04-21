use std::path::{Path, PathBuf};
use windows::core::HSTRING;
use windows::Win32::Storage::FileSystem::DeleteFileW;

pub fn install_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("BetWall")
}

pub fn target_exe() -> PathBuf {
    install_dir().join("betwall.exe")
}

fn strip_zone_identifier(p: &Path) {
    let ads = format!("{}:Zone.Identifier", p.display());
    let wide = HSTRING::from(ads);
    unsafe {
        let _ = DeleteFileW(&wide);
    }
}

pub fn install_if_needed() -> bool {
    let Ok(current) = std::env::current_exe() else {
        return false;
    };
    let target = target_exe();
    let current_canon = std::fs::canonicalize(&current).unwrap_or(current.clone());
    let target_canon = std::fs::canonicalize(&target).unwrap_or(target.clone());
    if current_canon == target_canon {
        strip_zone_identifier(&target);
        return false;
    }

    let dir = install_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    if std::fs::copy(&current, &target).is_err() {
        return false;
    }
    strip_zone_identifier(&target);

    let _ = std::process::Command::new(&target).spawn();
    true
}
