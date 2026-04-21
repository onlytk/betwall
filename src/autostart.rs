use std::env;
use std::path::PathBuf;
use windows::core::HSTRING;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};

const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "BetWall";

fn startup_folder() -> Option<PathBuf> {
    let appdata = env::var_os("APPDATA")?;
    let p = PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Startup");
    Some(p)
}

fn write_vbs_shortcut(exe_path: &str) {
    let Some(dir) = startup_folder() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let vbs_path = dir.join("BetWall.vbs");
    let escaped = exe_path.replace('"', "\"\"");
    let content = format!(
        "' BetWall autostart\r\nCreateObject(\"Wscript.Shell\").Run \"\"\"{escaped}\"\"\", 0, False\r\n"
    );
    let _ = std::fs::write(vbs_path, content);
}

fn write_registry(exe_path: &str) {
    let quoted = format!("\"{exe_path}\"");
    unsafe {
        let mut hkey = HKEY::default();
        let subkey = HSTRING::from(RUN_SUBKEY);
        let value = HSTRING::from(VALUE_NAME);

        let result = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &subkey,
            0,
            windows::core::PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_READ,
            None,
            &mut hkey,
            None,
        );
        if result.is_err() {
            return;
        }

        let wide: Vec<u16> = quoted.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes: &[u8] =
            std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
        let _ = RegSetValueExW(hkey, &value, 0, REG_SZ, Some(bytes));
        let _ = RegCloseKey(hkey);
    }
}

pub fn ensure_enabled() {
    let Ok(exe) = env::current_exe() else { return };
    let Some(path_str) = exe.to_str() else { return };
    write_registry(path_str);
    write_vbs_shortcut(path_str);
}
