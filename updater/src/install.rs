//! Installing a downloaded, verified release file.
//!
//! * deb / rpm / Arch: `pkexec apt-get install -y <files>` (or `dpkg -i`),
//!   `pkexec dnf install -y <files>`, `pkexec pacman -U --noconfirm <files>`.
//!   The packages' own scripts restart the capture service
//!   (`systemctl try-restart lunaris-capture.service`).
//! * AppImage: the new file is copied beside the old one and renamed over
//!   it (atomic, same folder, the old mode kept with +x). No root needed.
//! * Windows: a hidden PowerShell helper waits until this program has
//!   exited, runs the NSIS installer elevated and silently (`/S /D=<folder>`,
//!   UAC asks once), writes the outcome to a file the next start reads, and
//!   starts the program again. The installers stop and start the agent's
//!   Windows service around the copy.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::kind::{find_program, PackageManager};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// The password prompt (pkexec / UAC) was dismissed.
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::Cancelled => write!(f, "The update was cancelled"),
            InstallError::Failed(m) => write!(f, "The update could not be installed: {m}"),
        }
    }
}

/// A new folder only this user can read, for the downloads:
/// `<temp>/lunaris-update-<app>-<random>`.
pub fn private_dir(app: crate::App) -> std::io::Result<PathBuf> {
    let mut rnd = [0u8; 8];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut rnd)
        .map_err(|_| std::io::Error::other("no random source"))?;
    let name: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
    let dir = std::env::temp_dir().join(format!("{}{name}", dir_prefix(app)));
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(&dir)?; // fails when it exists: never a folder made by someone else
    Ok(dir)
}

pub fn dir_prefix(app: crate::App) -> String {
    format!("lunaris-update-{}-", app.id())
}

/// Removes download folders left by earlier updates of `app`.
pub fn remove_old_dirs(app: crate::App) {
    let prefix = dir_prefix(app);
    if let Ok(dir) = std::fs::read_dir(std::env::temp_dir()) {
        for e in dir.flatten() {
            let ours = e.file_name().to_str().is_some_and(|n| n.starts_with(&prefix));
            // symlink_metadata: a link named like ours is not followed.
            if ours && e.path().symlink_metadata().is_ok_and(|m| m.is_dir()) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

/// The pkexec command line for `files` (pure; `find` looks a program up).
pub fn package_command(
    m: PackageManager,
    files: &[PathBuf],
    find: &dyn Fn(&str) -> Option<PathBuf>,
) -> Result<(PathBuf, Vec<OsString>), String> {
    let pkexec = find("pkexec").ok_or("pkexec is not installed (package polkit)")?;
    let mut args: Vec<OsString> = Vec::new();
    match m {
        PackageManager::Dpkg => {
            // apt-get also installs new dependencies; plain dpkg would stop
            // at "dependency problems".
            if let Some(apt) = find("apt-get") {
                args.push(apt.into());
                args.extend(["install".into(), "-y".into()]);
            } else {
                let dpkg = find("dpkg").ok_or("neither apt-get nor dpkg is installed")?;
                args.push(dpkg.into());
                args.push("-i".into());
            }
        }
        PackageManager::Rpm => {
            if let Some(dnf) = find("dnf").or_else(|| find("dnf5")) {
                args.push(dnf.into());
                args.extend(["install".into(), "-y".into()]);
            } else if let Some(zypper) = find("zypper") {
                args.push(zypper.into());
                args.extend(["--non-interactive".into(), "install".into()]);
            } else {
                let rpm = find("rpm").ok_or("dnf is not installed")?;
                args.push(rpm.into());
                args.push("-U".into());
            }
        }
        PackageManager::Pacman => {
            let pacman = find("pacman").ok_or("pacman is not installed")?;
            args.push(pacman.into());
            args.extend(["-U".into(), "--noconfirm".into()]);
        }
    }
    for f in files {
        if !f.is_absolute() {
            return Err(format!("not an absolute path: {}", f.display()));
        }
        args.push(f.clone().into());
    }
    Ok((pkexec, args))
}

/// The outcome of the pkexec command (pure). pkexec exits 126 when the
/// password prompt was dismissed and 127 when authorisation failed.
pub fn classify_exit(code: Option<i32>, stderr: &str) -> Result<(), InstallError> {
    match code {
        Some(0) => Ok(()),
        Some(126 | 127) if stderr.contains("No authentication agent") => Err(InstallError::Failed(
            "no password prompt is available (polkit authentication agent)".into(),
        )),
        Some(126 | 127) => Err(InstallError::Cancelled),
        Some(c) => {
            let tail: String = {
                let t = stderr.trim();
                let start = t.char_indices().rev().nth(399).map(|(i, _)| i).unwrap_or(0);
                t[start..].to_string()
            };
            Err(InstallError::Failed(if tail.is_empty() {
                format!("exit code {c}")
            } else {
                format!("exit code {c}: {tail}")
            }))
        }
        None => Err(InstallError::Failed("the installer was stopped".into())),
    }
}

pub fn install_packages(m: PackageManager, files: &[PathBuf]) -> Result<(), InstallError> {
    let (pkexec, args) = package_command(m, files, &find_program).map_err(InstallError::Failed)?;
    let out = std::process::Command::new(pkexec)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| InstallError::Failed(format!("cannot start pkexec: {e}")))?;
    classify_exit(out.status.code(), &String::from_utf8_lossy(&out.stderr))
}

/// Puts `new` in place of the AppImage `target`: copy beside it, then
/// rename over it (atomic), keeping the old mode with execute bits.
pub fn replace_appimage(new: &Path, target: &Path) -> Result<(), InstallError> {
    let fail = |what: &str, e: std::io::Error| InstallError::Failed(format!("{what}: {e}"));
    let target = std::fs::canonicalize(target).map_err(|e| fail("cannot find the AppImage", e))?;
    let dir = target.parent().ok_or_else(|| InstallError::Failed("no folder".into()))?;
    let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("Lunaris.AppImage");
    let tmp = dir.join(format!(".{name}.update-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let result = (|| {
        std::fs::copy(new, &tmp).map_err(|e| fail(&format!("cannot write to {}", dir.display()), e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let old = std::fs::metadata(&target).map(|m| m.permissions().mode()).unwrap_or(0o755);
            // The old permission bits (never set-id) plus execute.
            let mode = (old & 0o777) | executable_bits(old);
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
                .map_err(|e| fail("cannot make the AppImage executable", e))?;
        }
        std::fs::File::open(&tmp).and_then(|f| f.sync_all()).map_err(|e| fail("cannot write", e))?;
        std::fs::rename(&tmp, &target).map_err(|e| fail(&format!("cannot replace {}", target.display()), e))?;
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// x for everyone who can read the old file, and always for the owner.
pub fn executable_bits(old_mode: u32) -> u32 {
    let mut x = 0o100;
    if old_mode & 0o040 != 0 {
        x |= 0o010;
    }
    if old_mode & 0o004 != 0 {
        x |= 0o001;
    }
    x
}

/// One PowerShell single-quoted string.
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The Windows helper (pure, see the module notes). `relaunch_args` are
/// passed to the restarted program.
pub fn windows_helper_script(pid: u32, installer: &str, install_dir: &str, exe: &str, relaunch_args: &[String], result_file: &str) -> String {
    // NSIS: `/D=` must be the last argument and is not quoted, even with spaces.
    let installer_args = format!("/S /D={install_dir}");
    let relaunch = if relaunch_args.is_empty() {
        String::new()
    } else {
        format!(" -ArgumentList {}", ps_quote(&relaunch_args.iter().map(|a| format!("\"{a}\"")).collect::<Vec<_>>().join(" ")))
    };
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         try {{ Wait-Process -Id {pid} -Timeout 120 -ErrorAction SilentlyContinue }} catch {{}}\n\
         $result = 'failed'\n\
         try {{\n\
         \x20 $p = Start-Process -FilePath {installer} -ArgumentList {args} -Verb RunAs -Wait -PassThru\n\
         \x20 if ($p.ExitCode -eq 0) {{ $result = 'ok' }} else {{ $result = 'failed ' + $p.ExitCode }}\n\
         }} catch {{\n\
         \x20 $e = $_.Exception; while ($e.InnerException) {{ $e = $e.InnerException }}\n\
         \x20 if ($e -is [System.ComponentModel.Win32Exception] -and $e.NativeErrorCode -eq 1223) {{ $result = 'cancelled' }} else {{ $result = 'failed ' + $e.Message }}\n\
         }}\n\
         try {{ Set-Content -LiteralPath {result} -Value $result -Encoding UTF8 }} catch {{}}\n\
         Start-Process -FilePath {exe}{relaunch}\n",
        installer = ps_quote(installer),
        args = ps_quote(&installer_args),
        result = ps_quote(result_file),
        exe = ps_quote(exe),
    )
}

/// The outcome the helper wrote (pure): Ok, or why it failed.
pub fn parse_windows_result(text: &str) -> Result<(), InstallError> {
    let t = text.trim().trim_start_matches('\u{feff}');
    match t {
        "ok" => Ok(()),
        "cancelled" => Err(InstallError::Cancelled),
        other => Err(InstallError::Failed(
            other.strip_prefix("failed").map(str::trim).filter(|s| !s.is_empty()).unwrap_or("the installer failed").to_string(),
        )),
    }
}

/// Starts the Windows helper (see [`windows_helper_script`]) and returns:
/// the caller then quits so the installer can replace the files.
#[cfg(windows)]
pub fn launch_windows_installer(installer: &Path, install_dir: &Path, exe: &Path, relaunch_args: &[String], result_file: &Path) -> Result<(), InstallError> {
    use base64::Engine as _;
    use std::os::windows::process::CommandExt;
    let script = windows_helper_script(
        std::process::id(),
        &installer.to_string_lossy(),
        &install_dir.to_string_lossy(),
        &exe.to_string_lossy(),
        relaunch_args,
        &result_file.to_string_lossy(),
    );
    let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    let system = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\Windows"));
    let powershell = system.join("System32\\WindowsPowerShell\\v1.0\\powershell.exe");
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    std::process::Command::new(powershell)
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-EncodedCommand"])
        .arg(encoded)
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| InstallError::Failed(format!("cannot start the installer helper: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finder(have: &'static [&'static str]) -> impl Fn(&str) -> Option<PathBuf> {
        move |n: &str| have.contains(&n).then(|| PathBuf::from(format!("/usr/bin/{n}")))
    }

    fn strs(args: &[OsString]) -> Vec<String> {
        args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn package_commands() {
        let files = vec![PathBuf::from("/tmp/d/lunaris-client_0.6.4_amd64.deb"), PathBuf::from("/tmp/d/lunaris-agent_0.6.4_amd64.deb")];
        let (p, a) = package_command(PackageManager::Dpkg, &files, &finder(&["pkexec", "apt-get", "dpkg"])).unwrap();
        assert_eq!(p, PathBuf::from("/usr/bin/pkexec"));
        assert_eq!(strs(&a), ["/usr/bin/apt-get", "install", "-y", "/tmp/d/lunaris-client_0.6.4_amd64.deb", "/tmp/d/lunaris-agent_0.6.4_amd64.deb"]);
        let (_, a) = package_command(PackageManager::Dpkg, &files[..1], &finder(&["pkexec", "dpkg"])).unwrap();
        assert_eq!(strs(&a), ["/usr/bin/dpkg", "-i", "/tmp/d/lunaris-client_0.6.4_amd64.deb"]);
        let rpm = vec![PathBuf::from("/tmp/d/x.rpm")];
        let (_, a) = package_command(PackageManager::Rpm, &rpm, &finder(&["pkexec", "dnf", "rpm"])).unwrap();
        assert_eq!(strs(&a), ["/usr/bin/dnf", "install", "-y", "/tmp/d/x.rpm"]);
        let zst = vec![PathBuf::from("/tmp/d/x.pkg.tar.zst")];
        let (_, a) = package_command(PackageManager::Pacman, &zst, &finder(&["pkexec", "pacman"])).unwrap();
        assert_eq!(strs(&a), ["/usr/bin/pacman", "-U", "--noconfirm", "/tmp/d/x.pkg.tar.zst"]);
        assert!(package_command(PackageManager::Pacman, &zst, &finder(&["pacman"])).is_err(), "no pkexec");
        assert!(package_command(PackageManager::Pacman, &[PathBuf::from("x.zst")], &finder(&["pkexec", "pacman"])).is_err());
    }

    #[test]
    fn pkexec_exit_codes() {
        assert_eq!(classify_exit(Some(0), ""), Ok(()));
        assert_eq!(classify_exit(Some(126), ""), Err(InstallError::Cancelled));
        assert_eq!(classify_exit(Some(127), "Error executing command as another user: Not authorized"), Err(InstallError::Cancelled));
        assert!(matches!(classify_exit(Some(127), "No authentication agent found."), Err(InstallError::Failed(_))));
        assert_eq!(classify_exit(Some(100), "E: broken\n"), Err(InstallError::Failed("exit code 100: E: broken".into())));
        assert_eq!(InstallError::Cancelled.to_string(), "The update was cancelled");
    }

    #[test]
    fn modes() {
        assert_eq!(executable_bits(0o644), 0o111);
        assert_eq!(executable_bits(0o600), 0o100);
        assert_eq!(executable_bits(0o640), 0o110);
    }

    #[cfg(unix)]
    #[test]
    fn appimage_is_replaced_in_place() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("lunaris-updater-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("Lunaris.AppImage");
        let new = dir.join("new");
        std::fs::write(&target, b"old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o750)).unwrap();
        std::fs::write(&new, b"new version").unwrap();
        replace_appimage(&new, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new version");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o110, 0o110, "{mode:o}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2, "no temporary file left");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn windows_script() {
        let s = windows_helper_script(
            42,
            "C:\\Users\\o'neil\\AppData\\Local\\Temp\\lunaris-update-client-ab\\lunaris-client-installer-windows.exe",
            "C:\\Program Files\\Lunaris Client",
            "C:\\Program Files\\Lunaris Client\\client-desktop.exe",
            &[],
            "C:\\r.txt",
        );
        assert!(s.contains("Wait-Process -Id 42"));
        assert!(s.contains("'C:\\Users\\o''neil\\"), "quote escaped");
        assert!(s.contains("-ArgumentList '/S /D=C:\\Program Files\\Lunaris Client' -Verb RunAs -Wait -PassThru"));
        assert!(s.contains("Start-Process -FilePath 'C:\\Program Files\\Lunaris Client\\client-desktop.exe'\n"));
        let s = windows_helper_script(1, "i", "d", "e", &["--config".into(), "C:\\a b.json".into()], "r");
        assert!(s.contains("Start-Process -FilePath 'e' -ArgumentList '\"--config\" \"C:\\a b.json\"'"));
    }

    #[test]
    fn windows_results() {
        assert_eq!(parse_windows_result("\u{feff}ok\r\n"), Ok(()));
        assert_eq!(parse_windows_result("cancelled"), Err(InstallError::Cancelled));
        assert_eq!(parse_windows_result("failed 2"), Err(InstallError::Failed("2".into())));
        assert_eq!(parse_windows_result("failed"), Err(InstallError::Failed("the installer failed".into())));
    }
}
