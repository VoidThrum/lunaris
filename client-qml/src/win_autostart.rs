//! Starting with Windows, for an app that runs as administrator.
//!
//! The Windows app asks for administrator rights (its manifest says
//! `requireAdministrator`; see `client-desktop/build.rs`), because a process
//! without them cannot send input to windows of elevated programs (Task
//! Manager, installers, an elevated terminal: User Interface Privilege
//! Isolation). Windows does not start such a program from the `Run` key at
//! logon: it would need a consent prompt and there is nobody to answer yet,
//! so the entry is skipped. A scheduled task that runs "with highest
//! privileges" at the user's logon is the documented way to start an
//! elevated program without a prompt, and creating one needs the rights the
//! app already has.
//!
//! The task is created from XML rather than from `schtasks /Create` switches:
//! the switches leave the defaults "start only on AC power" and "stop after
//! 3 days", which would end a remote-control app on a laptop and after a
//! long weekend.

/// The task's name in the Task Scheduler.
pub const TASK_NAME: &str = "LunarisClient";

/// The value of the old `Run` entry (apps before the scheduled task).
pub const RUN_VALUE: &str = "LunarisClient";

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// The task: at `user`'s logon, start `exe --minimized` in their session
/// with the highest rights they have, with no time limit and on battery too.
/// (Task Scheduler schema 1.2, the one Windows 7 and later read.)
pub fn task_xml(exe: &str, user: &str) -> String {
    let (exe, user) = (xml_escape(exe), xml_escape(user));
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts Lunaris Client when you sign in.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>--minimized</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// `text` as `schtasks /XML` reads it: UTF-16, little endian, with the mark.
pub fn utf16_file(text: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

#[cfg(target_os = "windows")]
fn quiet(program: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = std::process::Command::new(program);
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

#[cfg(target_os = "windows")]
fn run_entry(action: &str) -> bool {
    let mut cmd = quiet("reg");
    cmd.args([action, "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run", "/v", RUN_VALUE]);
    if action == "delete" {
        cmd.arg("/f");
    }
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

/// Whether the app starts with Windows (by the task, or by the `Run` entry
/// of an older app, which `enable` replaces).
#[cfg(target_os = "windows")]
pub fn is_enabled() -> bool {
    let task = quiet("schtasks").args(["/Query", "/TN", TASK_NAME]).output().map(|o| o.status.success()).unwrap_or(false);
    task || run_entry("query")
}

#[cfg(target_os = "windows")]
pub fn enable(exe: &str) -> Result<(), String> {
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(domain), Ok(name)) if !domain.is_empty() => format!("{}\\{}", domain, name),
        (_, Ok(name)) => name,
        _ => return Err("the user's name is not known".into()),
    };
    let file = std::env::temp_dir().join(format!("lunaris-autostart-{}.xml", std::process::id()));
    std::fs::write(&file, utf16_file(&task_xml(exe, &user))).map_err(|e| e.to_string())?;
    let out = quiet("schtasks").args(["/Create", "/TN", TASK_NAME, "/F", "/XML"]).arg(&file).output();
    let _ = std::fs::remove_file(&file);
    match out {
        Ok(o) if o.status.success() => {
            // The task has taken over from the old entry.
            run_entry("delete");
            Ok(())
        }
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(target_os = "windows")]
pub fn disable() {
    let _ = quiet("schtasks").args(["/Delete", "/TN", TASK_NAME, "/F"]).output();
    run_entry("delete");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_task_runs_elevated_at_logon_without_limits() {
        let xml = task_xml(r"C:\Program Files\Lunaris Client\client-desktop.exe", r"PC\huy");
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
        assert!(xml.contains(r"<Command>C:\Program Files\Lunaris Client\client-desktop.exe</Command>"));
        assert!(xml.contains("<Arguments>--minimized</Arguments>"));
        assert_eq!(xml.matches(r"<UserId>PC\huy</UserId>").count(), 2);
    }

    #[test]
    fn names_with_markup_characters_stay_text() {
        let xml = task_xml(r"C:\A&B\<x>.exe", "O'Neil \"Q\"");
        assert!(xml.contains(r"<Command>C:\A&amp;B\&lt;x&gt;.exe</Command>"));
        assert!(xml.contains("<UserId>O&apos;Neil &quot;Q&quot;</UserId>"));
    }

    #[test]
    fn the_file_is_utf16_with_its_mark() {
        assert_eq!(utf16_file("<é"), [0xFF, 0xFE, b'<', 0, 0xE9, 0]);
    }
}
