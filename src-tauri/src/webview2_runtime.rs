#[cfg(target_os = "windows")]
use std::path::{Path, PathBuf};

#[cfg(target_os = "windows")]
const WEBVIEW2_RUNTIME_ENV: &str = "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER";
#[cfg(target_os = "windows")]
const WEBVIEW2_EXE: &str = "msedgewebview2.exe";
#[cfg(target_os = "windows")]
const BUNDLED_WEBVIEW2_MARKER: &str = "bundled-webview2.dbx";
#[cfg(target_os = "windows")]
const WEBVIEW2_INSTALLER_MARKER: &str = "webview2-installer.dbx";
#[cfg(target_os = "windows")]
const WEBVIEW2_INSTALLER_EXE: &str = "MicrosoftEdgeWebView2RuntimeInstallerX64.exe";
#[cfg(target_os = "windows")]
const WEBVIEW2_INSTALL_RECORD: &str = "dbx-webview2-install.dbx";
#[cfg(target_os = "windows")]
const MESSAGE_BOX_YES: i32 = 6;
#[cfg(target_os = "windows")]
const MESSAGE_BOX_NO: i32 = 7;
#[cfg(target_os = "windows")]
const MESSAGE_BOX_OK: u32 = 0x0000_0000;
#[cfg(target_os = "windows")]
const MESSAGE_BOX_YES_NO_CANCEL_ICON_WARNING: u32 = 0x0000_0003 | 0x0000_0030;
#[cfg(target_os = "windows")]
const MESSAGE_BOX_OK_ICON_INFO: u32 = MESSAGE_BOX_OK | 0x0000_0040;
#[cfg(target_os = "windows")]
const MESSAGE_BOX_OK_ICON_ERROR: u32 = MESSAGE_BOX_OK | 0x0000_0010;

#[cfg(target_os = "windows")]
pub fn handle_webview2_uninstall_arg() -> bool {
    if !std::env::args().any(|arg| arg == "--uninstall-webview2") {
        return false;
    }
    let Some(exe_dir) = current_exe_dir() else {
        show_message_box("DBX WebView2 卸载", "无法定位 DBX 目录，未执行卸载。", MESSAGE_BOX_OK_ICON_ERROR);
        return true;
    };
    let message = uninstall_dbx_installed_webview2(&exe_dir);
    let icon = if message.starts_with("已") { MESSAGE_BOX_OK_ICON_INFO } else { MESSAGE_BOX_OK_ICON_ERROR };
    show_message_box("DBX WebView2 卸载", &message, icon);
    true
}

#[cfg(not(target_os = "windows"))]
pub fn handle_webview2_uninstall_arg() -> bool {
    false
}

#[cfg(target_os = "windows")]
pub fn configure_bundled_webview2_fallback() {
    if let Some(existing_runtime_dir) = std::env::var_os(WEBVIEW2_RUNTIME_ENV).filter(|value| !value.is_empty()) {
        if let Some(exe_dir) = current_exe_dir() {
            write_startup_log(
                &exe_dir,
                &[
                    format!("{WEBVIEW2_RUNTIME_ENV} already set: {}", PathBuf::from(existing_runtime_dir).display()),
                    "skip DBX WebView2 runtime configuration".to_string(),
                ],
            );
        }
        return;
    }

    let Some(exe_dir) = current_exe_dir() else {
        return;
    };

    let bundled_runtime_preferred = bundled_webview2_marker_exists(&exe_dir);
    let bundled_runtime_dir = bundled_webview2_runtime_dir(&exe_dir);
    let installer_path = webview2_installer_path(&exe_dir);
    let mut system_runtime_dirs = system_webview2_runtime_dirs();
    let install_result = if system_runtime_dirs.is_empty() {
        install_webview2_runtime_if_available(installer_path.as_deref())
    } else {
        None
    };
    if install_result.is_some() {
        system_runtime_dirs = system_webview2_runtime_dirs();
    }
    let runtime_dir = webview2_runtime_dir_for_startup(&exe_dir, !system_runtime_dirs.is_empty());

    let mut log_lines = vec![
        format!("exe_dir={}", exe_dir.display()),
        format!("bundled_marker_exists={bundled_runtime_preferred}"),
        format!(
            "bundled_runtime_dir={}",
            bundled_runtime_dir.as_ref().map(|path| path.display().to_string()).unwrap_or_else(|| "<none>".to_string())
        ),
        format!(
            "installer_path={}",
            installer_path.as_ref().map(|path| path.display().to_string()).unwrap_or_else(|| "<none>".to_string())
        ),
        format!("installer_result={}", install_result.as_deref().unwrap_or("<not run>")),
        format!("system_runtime_count={}", system_runtime_dirs.len()),
    ];
    log_lines.extend(system_runtime_dirs.iter().map(|path| format!("system_runtime_dir={}", path.display())));

    if let Some(runtime_dir) = runtime_dir.as_ref() {
        std::env::set_var(WEBVIEW2_RUNTIME_ENV, &runtime_dir);
        log_lines.push(format!("selected_runtime_dir={}", runtime_dir.display()));
    } else {
        log_lines.push("selected_runtime_dir=<system default>".to_string());
    }
    log_lines.push(format!(
        "final_{WEBVIEW2_RUNTIME_ENV}={}",
        std::env::var_os(WEBVIEW2_RUNTIME_ENV)
            .map(PathBuf::from)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<not set>".to_string())
    ));
    write_startup_log(&exe_dir, &log_lines);

    if should_show_missing_webview2_prompt(system_runtime_dirs.len(), runtime_dir.is_some(), installer_path.is_some()) {
        show_message_box(
            "DBX 缺少 WebView2",
            "本机未检测到 Microsoft Edge WebView2 Runtime。\n\n\
当前 DBX 包不包含 WebView2 安装器，无法自动安装。\n\n\
请使用带 WebView2 的 DBX 包，或先手动安装 Microsoft Edge WebView2 Runtime 后再启动 DBX。",
            MESSAGE_BOX_OK_ICON_ERROR,
        );
        std::process::exit(1);
    }

    if matches!(install_result.as_deref(), Some("cancelled_by_user"))
        && system_runtime_dirs.is_empty()
        && runtime_dir.is_none()
    {
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "windows"))]
pub fn configure_bundled_webview2_fallback() {}

#[cfg(target_os = "windows")]
fn bundled_webview2_runtime_dir(exe_dir: &Path) -> Option<PathBuf> {
    let runtime_dir = exe_dir.join("WebView2");
    runtime_dir.join(WEBVIEW2_EXE).is_file().then_some(runtime_dir)
}

#[cfg(target_os = "windows")]
fn current_exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|path| path.parent().map(Path::to_path_buf))
}

#[cfg(target_os = "windows")]
fn bundled_webview2_marker_exists(exe_dir: &Path) -> bool {
    exe_dir.join(BUNDLED_WEBVIEW2_MARKER).is_file()
}

#[cfg(target_os = "windows")]
fn webview2_installer_path(exe_dir: &Path) -> Option<PathBuf> {
    if !exe_dir.join(WEBVIEW2_INSTALLER_MARKER).is_file() {
        return None;
    }
    let installer_path = exe_dir.join("WebView2Installer").join(WEBVIEW2_INSTALLER_EXE);
    installer_path.is_file().then_some(installer_path)
}

#[cfg(target_os = "windows")]
fn install_webview2_runtime_if_available(installer_path: Option<&Path>) -> Option<String> {
    let installer_path = installer_path?;
    let choice = prompt_webview2_install_choice();
    if choice == WebView2InstallChoice::Cancel {
        return Some("cancelled_by_user".to_string());
    }
    let output = match choice {
        WebView2InstallChoice::CurrentUser => run_webview2_installer_for_current_user(installer_path),
        WebView2InstallChoice::AllUsers => run_webview2_installer_for_all_users(installer_path),
        WebView2InstallChoice::Cancel => unreachable!(),
    };
    Some(match output {
        Ok(output) => {
            if output.status.success() {
                let _ = write_webview2_install_record(installer_path, choice);
            }
            format!(
                "choice={}; exit_code={:?}; stdout={}; stderr={}",
                choice.as_log_value(),
                output.status.code(),
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            )
        }
        Err(err) => format!("choice={}; failed_to_start={err}", choice.as_log_value()),
    })
}

#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebView2InstallChoice {
    AllUsers,
    CurrentUser,
    Cancel,
}

#[cfg(target_os = "windows")]
impl WebView2InstallChoice {
    fn as_log_value(self) -> &'static str {
        match self {
            WebView2InstallChoice::AllUsers => "all_users",
            WebView2InstallChoice::CurrentUser => "current_user",
            WebView2InstallChoice::Cancel => "cancel",
        }
    }
}

#[cfg(target_os = "windows")]
fn prompt_webview2_install_choice() -> WebView2InstallChoice {
    match show_webview2_install_prompt() {
        MESSAGE_BOX_YES => WebView2InstallChoice::AllUsers,
        MESSAGE_BOX_NO => WebView2InstallChoice::CurrentUser,
        _ => WebView2InstallChoice::Cancel,
    }
}

#[cfg(target_os = "windows")]
fn show_webview2_install_prompt() -> i32 {
    show_message_box(
        "DBX 需要安装 WebView2",
        "本机未检测到 Microsoft Edge WebView2 Runtime，DBX 需要先安装 WebView2 才能启动。\n\n\
请选择安装方式：\n\
是(Y)：为所有用户安装，需要管理员权限\n\
否(N)：仅为当前用户安装\n\
取消：不安装并退出 DBX",
        MESSAGE_BOX_YES_NO_CANCEL_ICON_WARNING,
    )
}

#[cfg(target_os = "windows")]
fn show_message_box(caption: &str, text: &str, typ: u32) -> i32 {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(hwnd: isize, text: *const u16, caption: *const u16, typ: u32) -> i32;
    }

    let text = OsStr::new(text).encode_wide().chain(std::iter::once(0)).collect::<Vec<_>>();
    let caption = OsStr::new(caption).encode_wide().chain(std::iter::once(0)).collect::<Vec<_>>();

    unsafe { MessageBoxW(0, text.as_ptr(), caption.as_ptr(), typ) }
}

#[cfg(target_os = "windows")]
fn run_webview2_installer_for_current_user(installer_path: &Path) -> std::io::Result<std::process::Output> {
    std::process::Command::new(installer_path).args(["/silent", "/install"]).output()
}

#[cfg(target_os = "windows")]
fn run_webview2_installer_for_all_users(installer_path: &Path) -> std::io::Result<std::process::Output> {
    std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &webview2_elevated_install_script(installer_path),
        ])
        .output()
}

#[cfg(target_os = "windows")]
fn write_webview2_install_record(installer_path: &Path, choice: WebView2InstallChoice) -> std::io::Result<()> {
    let Some(exe_dir) = installer_path.parent().and_then(Path::parent) else {
        return Ok(());
    };
    let content = format!("scope={}\n", choice.as_log_value());
    std::fs::write(exe_dir.join(WEBVIEW2_INSTALL_RECORD), content)
}

#[cfg(target_os = "windows")]
fn read_webview2_install_record(exe_dir: &Path) -> Option<WebView2InstallChoice> {
    let content = std::fs::read_to_string(exe_dir.join(WEBVIEW2_INSTALL_RECORD)).ok()?;
    for line in content.lines() {
        match line.trim() {
            "scope=all_users" => return Some(WebView2InstallChoice::AllUsers),
            "scope=current_user" => return Some(WebView2InstallChoice::CurrentUser),
            _ => {}
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn uninstall_dbx_installed_webview2(exe_dir: &Path) -> String {
    let Some(scope) = read_webview2_install_record(exe_dir) else {
        return "未发现 DBX 的 WebView2 安装记录。为避免影响其他程序，不会卸载本机已有 WebView2。".to_string();
    };
    let Some(setup_path) = webview2_setup_path_for_uninstall(scope) else {
        return "未找到 WebView2 卸载程序，未执行卸载。".to_string();
    };
    let output = match scope {
        WebView2InstallChoice::CurrentUser => run_webview2_uninstaller_for_current_user(&setup_path),
        WebView2InstallChoice::AllUsers => run_webview2_uninstaller_for_all_users(&setup_path),
        WebView2InstallChoice::Cancel => unreachable!(),
    };
    match output {
        Ok(output) if output.status.success() => {
            let _ = std::fs::remove_file(exe_dir.join(WEBVIEW2_INSTALL_RECORD));
            "已卸载 DBX 安装的 WebView2 Runtime。".to_string()
        }
        Ok(output) => format!(
            "WebView2 卸载失败，退出码：{:?}\n{}{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(err) => format!("WebView2 卸载程序启动失败：{err}"),
    }
}

#[cfg(target_os = "windows")]
fn webview2_setup_path_for_uninstall(scope: WebView2InstallChoice) -> Option<PathBuf> {
    let dirs = system_webview2_runtime_dirs();
    let preferred_local = scope == WebView2InstallChoice::CurrentUser;
    dirs.into_iter()
        .filter(|path| path.join("Installer").join("setup.exe").is_file())
        .find(|path| {
            let text = path.to_string_lossy().to_ascii_lowercase();
            let is_local = text.contains("\\appdata\\local\\");
            is_local == preferred_local
        })
        .or_else(|| {
            system_webview2_runtime_dirs().into_iter().find(|path| path.join("Installer").join("setup.exe").is_file())
        })
        .map(|path| path.join("Installer").join("setup.exe"))
}

#[cfg(target_os = "windows")]
fn run_webview2_uninstaller_for_current_user(setup_path: &Path) -> std::io::Result<std::process::Output> {
    std::process::Command::new(setup_path).args(["--uninstall", "--msedgewebview", "--force-uninstall"]).output()
}

#[cfg(target_os = "windows")]
fn run_webview2_uninstaller_for_all_users(setup_path: &Path) -> std::io::Result<std::process::Output> {
    std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &webview2_elevated_uninstall_script(setup_path)])
        .output()
}

#[cfg(target_os = "windows")]
fn webview2_elevated_install_script(installer_path: &Path) -> String {
    format!(
        "$p = Start-Process -FilePath {} -ArgumentList '/silent','/install' -Verb RunAs -Wait -PassThru; if ($null -ne $p.ExitCode) {{ exit $p.ExitCode }}",
        powershell_single_quote(&installer_path.display().to_string())
    )
}

#[cfg(target_os = "windows")]
fn webview2_elevated_uninstall_script(setup_path: &Path) -> String {
    format!(
        "$p = Start-Process -FilePath {} -ArgumentList '--uninstall','--msedgewebview','--system-level','--force-uninstall' -Verb RunAs -Wait -PassThru; if ($null -ne $p.ExitCode) {{ exit $p.ExitCode }}",
        powershell_single_quote(&setup_path.display().to_string())
    )
}

#[cfg(target_os = "windows")]
fn powershell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(target_os = "windows")]
fn webview2_runtime_dir_for_startup(exe_dir: &Path, system_runtime_exists: bool) -> Option<PathBuf> {
    let bundled_runtime_dir = bundled_webview2_runtime_dir(exe_dir);
    if system_runtime_exists {
        None
    } else {
        bundled_runtime_dir
    }
}

#[cfg(target_os = "windows")]
fn should_show_missing_webview2_prompt(
    system_runtime_count: usize,
    bundled_runtime_available: bool,
    installer_available: bool,
) -> bool {
    system_runtime_count == 0 && !bundled_runtime_available && !installer_available
}

#[cfg(target_os = "windows")]
fn system_webview2_runtime_dirs() -> Vec<PathBuf> {
    system_webview2_runtime_dirs_from_env(|key| std::env::var_os(key))
}

#[cfg(target_os = "windows")]
fn system_webview2_runtime_dirs_from_env<F>(mut var_os: F) -> Vec<PathBuf>
where
    F: FnMut(&str) -> Option<std::ffi::OsString>,
{
    ["ProgramFiles(x86)", "ProgramFiles", "LocalAppData"]
        .into_iter()
        .filter_map(|key| var_os(key).map(PathBuf::from))
        .flat_map(|base| edge_webview_runtime_dirs(&base))
        .collect()
}

#[cfg(target_os = "windows")]
fn edge_webview_runtime_dirs(base: &Path) -> Vec<PathBuf> {
    let app_dir = base.join("Microsoft").join("EdgeWebView").join("Application");
    let Ok(entries) = std::fs::read_dir(app_dir) else {
        return Vec::new();
    };
    entries.filter_map(Result::ok).map(|entry| entry.path()).filter(|path| path.join(WEBVIEW2_EXE).is_file()).collect()
}

#[cfg(target_os = "windows")]
fn write_startup_log(exe_dir: &Path, lines: &[String]) {
    use std::io::Write;

    let log_path = exe_dir.join("dbx-webview2-startup.log");
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(log_path) else {
        return;
    };
    let _ = writeln!(file, "=== DBX WebView2 startup ===");
    let _ = writeln!(file, "timestamp={:?}", std::time::SystemTime::now());
    for line in lines {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use std::{ffi::OsString, path::PathBuf};

    use super::{
        bundled_webview2_runtime_dir, powershell_single_quote, read_webview2_install_record,
        should_show_missing_webview2_prompt, system_webview2_runtime_dirs_from_env, webview2_elevated_install_script,
        webview2_elevated_uninstall_script, webview2_installer_path, webview2_runtime_dir_for_startup,
        write_webview2_install_record, WebView2InstallChoice,
    };

    #[test]
    fn bundled_runtime_requires_msedgewebview2_exe() {
        let root = std::env::temp_dir().join(format!("dbx-webview2-test-{}", std::process::id()));
        let exe_dir = root.join("app");
        let runtime_dir = exe_dir.join("WebView2");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        assert_eq!(bundled_webview2_runtime_dir(&exe_dir), None);

        std::fs::write(runtime_dir.join("msedgewebview2.exe"), b"").unwrap();
        assert_eq!(bundled_webview2_runtime_dir(&exe_dir), Some(runtime_dir));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn system_runtime_takes_precedence_without_bundled_runtime_marker() {
        let root = std::env::temp_dir().join(format!("dbx-webview2-force-test-{}", std::process::id()));
        let exe_dir = root.join("app");
        let runtime_dir = exe_dir.join("WebView2");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::write(runtime_dir.join("msedgewebview2.exe"), b"").unwrap();
        assert_eq!(webview2_runtime_dir_for_startup(&exe_dir, true), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn system_runtime_takes_precedence_even_with_bundled_runtime_marker() {
        let root = std::env::temp_dir().join(format!("dbx-webview2-marker-test-{}", std::process::id()));
        let exe_dir = root.join("app");
        let runtime_dir = exe_dir.join("WebView2");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::write(runtime_dir.join("msedgewebview2.exe"), b"").unwrap();
        std::fs::write(exe_dir.join("bundled-webview2.dbx"), b"").unwrap();

        assert_eq!(webview2_runtime_dir_for_startup(&exe_dir, true), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bundled_runtime_is_used_when_system_runtime_is_missing() {
        let root = std::env::temp_dir().join(format!("dbx-webview2-fallback-test-{}", std::process::id()));
        let exe_dir = root.join("app");
        let runtime_dir = exe_dir.join("WebView2");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::write(runtime_dir.join("msedgewebview2.exe"), b"").unwrap();

        assert_eq!(webview2_runtime_dir_for_startup(&exe_dir, false), Some(runtime_dir));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn no_runtime_override_is_set_without_bundled_runtime() {
        let root = std::env::temp_dir().join(format!("dbx-webview2-none-test-{}", std::process::id()));
        let exe_dir = root.join("app");
        std::fs::create_dir_all(&exe_dir).unwrap();

        assert_eq!(webview2_runtime_dir_for_startup(&exe_dir, false), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn evergreen_installer_requires_marker_and_installer_exe() {
        let root = std::env::temp_dir().join(format!("dbx-webview2-installer-test-{}", std::process::id()));
        let exe_dir = root.join("app");
        let installer_dir = exe_dir.join("WebView2Installer");
        let installer_path = installer_dir.join("MicrosoftEdgeWebView2RuntimeInstallerX64.exe");
        std::fs::create_dir_all(&installer_dir).unwrap();

        assert_eq!(webview2_installer_path(&exe_dir), None);
        std::fs::write(exe_dir.join("webview2-installer.dbx"), b"").unwrap();
        assert_eq!(webview2_installer_path(&exe_dir), None);
        std::fs::write(&installer_path, b"").unwrap();
        assert_eq!(webview2_installer_path(&exe_dir), Some(installer_path));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_webview2_prompt_is_only_needed_without_system_runtime_bundled_runtime_or_installer() {
        assert!(should_show_missing_webview2_prompt(0, false, false));
        assert!(!should_show_missing_webview2_prompt(1, false, false));
        assert!(!should_show_missing_webview2_prompt(0, true, false));
        assert!(!should_show_missing_webview2_prompt(0, false, true));
    }

    #[test]
    fn system_runtime_checks_standard_install_roots() {
        let root = std::env::temp_dir().join(format!("dbx-webview2-env-test-{}", std::process::id()));
        let runtime_dir = root.join("Microsoft").join("EdgeWebView").join("Application").join("150.0.4078.48");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::write(runtime_dir.join("msedgewebview2.exe"), b"").unwrap();

        let dirs = system_webview2_runtime_dirs_from_env(|key| {
            (key == "ProgramFiles(x86)").then(|| OsString::from(PathBuf::from(&root)))
        });

        assert_eq!(dirs, vec![runtime_dir]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn powershell_single_quote_escapes_embedded_quotes() {
        assert_eq!(powershell_single_quote(r"C:\DBX's\WebView2.exe"), r"'C:\DBX''s\WebView2.exe'");
    }

    #[test]
    fn elevated_install_script_runs_installer_as_admin() {
        let path =
            PathBuf::from(r"C:\Program Files\DBX\WebView2Installer\MicrosoftEdgeWebView2RuntimeInstallerX64.exe");
        let script = webview2_elevated_install_script(&path);
        assert!(script.contains("Start-Process"));
        assert!(script.contains("-Verb RunAs"));
        assert!(script.contains("-Wait"));
        assert!(script.contains("'/silent','/install'"));
        assert!(
            script.contains(r"'C:\Program Files\DBX\WebView2Installer\MicrosoftEdgeWebView2RuntimeInstallerX64.exe'")
        );
    }

    #[test]
    fn install_record_round_trips_current_user_scope() {
        let root = std::env::temp_dir().join(format!("dbx-webview2-record-test-{}", std::process::id()));
        let installer_dir = root.join("WebView2Installer");
        let installer_path = installer_dir.join("MicrosoftEdgeWebView2RuntimeInstallerX64.exe");
        std::fs::create_dir_all(&installer_dir).unwrap();
        std::fs::write(&installer_path, b"").unwrap();

        write_webview2_install_record(&installer_path, WebView2InstallChoice::CurrentUser).unwrap();
        assert_eq!(read_webview2_install_record(&root), Some(WebView2InstallChoice::CurrentUser));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn elevated_uninstall_script_runs_setup_as_admin() {
        let path = PathBuf::from(
            r"C:\Program Files (x86)\Microsoft\EdgeWebView\Application\150.0.4078.48\Installer\setup.exe",
        );
        let script = webview2_elevated_uninstall_script(&path);
        assert!(script.contains("Start-Process"));
        assert!(script.contains("-Verb RunAs"));
        assert!(script.contains("'--uninstall','--msedgewebview','--system-level','--force-uninstall'"));
        assert!(script
            .contains(r"'C:\Program Files (x86)\Microsoft\EdgeWebView\Application\150.0.4078.48\Installer\setup.exe'"));
    }
}
