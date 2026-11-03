//! Session-scoped firewall rules for the Wi-Fi Direct interface.
//!
//! All privileged work happens in `monitorbeam-firewall-helper`, started through
//! `pkexec`. The helper removes the rules when the pipe to it closes, so a
//! crash of this program cannot leave them behind. Only the helper being killed
//! can, and then `restore_stale` finds them through the helper's marker file.

pub mod protocol;

use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};

use thiserror::Error;
use tracing::{debug, info, warn};

use protocol::{MARKER_PATH, Marker, is_valid_p2p_interface};

const PKEXEC: &str = "/usr/bin/pkexec";
/// Fixed and root-owned: the polkit policy authorizes exactly this path.
const HELPER_PATH: &str = "/usr/libexec/monitorbeam/monitorbeam-firewall-helper";
const READY: &str = "ready";
const UFW_CONFIG: &str = "/etc/ufw/ufw.conf";
const FIREWALL_CMD: &str = "firewall-cmd";
/// `pkexec` exit codes when authorization was dismissed or refused.
const PKEXEC_DISMISSED: i32 = 126;
const PKEXEC_NOT_AUTHORIZED: i32 = 127;
const MAX_LINE_LENGTH: u64 = 64;
const MAX_ERROR_LENGTH: u64 = 4096;

#[derive(Debug, Error)]
pub enum FirewallError {
    #[error("{0} is not a Wi-Fi Direct interface")]
    InvalidInterface(String),
    #[error("the firewall helper is not installed; see the README")]
    NotInstalled,
    #[error("administrator authorization was not given")]
    NotAuthorized,
    #[error("cannot run the firewall helper: {0}")]
    Io(#[from] io::Error),
    #[error("the firewall helper failed: {0}")]
    Helper(String),
}

/// An open firewall for one session. Rules last until `close` or drop.
#[derive(Debug)]
pub struct FirewallSession {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl FirewallSession {
    /// Opens the firewall on `interface`. May ask for the administrator
    /// password and so block for a long time: do not call on the GTK thread.
    pub fn open(interface: &str) -> Result<Self, FirewallError> {
        if !is_valid_p2p_interface(interface) {
            return Err(FirewallError::InvalidInterface(interface.to_owned()));
        }
        ensure_helper_installed(Path::new(HELPER_PATH))?;
        let session = Self::spawn(PKEXEC, &[HELPER_PATH, "open", interface])?;
        info!(interface, "firewall opened for session");
        Ok(session)
    }

    fn spawn(program: &str, args: &[&str]) -> Result<Self, FirewallError> {
        let mut child = helper_command(program, args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| FirewallError::Helper("no output pipe".to_owned()))?;

        let mut line = String::new();
        BufReader::new(stdout.take(MAX_LINE_LENGTH)).read_line(&mut line)?;
        if line.trim_end() == READY {
            return Ok(Self { child, stdin });
        }
        drop(stdin);
        let status = child.wait()?;
        Err(helper_failure(status, &mut child))
    }

    /// Removes the rules and reports whether that worked.
    pub fn close(mut self) -> Result<(), FirewallError> {
        self.end()
    }

    fn end(&mut self) -> Result<(), FirewallError> {
        // Closing the pipe is what tells the helper to remove the rules.
        drop(self.stdin.take());
        let status = self.child.wait()?;
        if status.success() {
            debug!("firewall closed");
            Ok(())
        } else {
            Err(helper_failure(status, &mut self.child))
        }
    }
}

impl Drop for FirewallSession {
    fn drop(&mut self) {
        if self.stdin.is_some()
            && let Err(error) = self.end()
        {
            warn!(%error, "cannot close the firewall");
        }
    }
}

/// Keeps a Ctrl+C in the terminal from reaching the helper together with this
/// program, because the helper still has to remove the rules afterwards.
fn helper_command(program: &str, args: &[&str]) -> Command {
    use std::os::unix::process::CommandExt as _;

    let mut command = Command::new(program);
    command.args(args).process_group(0);
    command
}

fn ensure_helper_installed(path: &Path) -> Result<(), FirewallError> {
    if path.exists() {
        Ok(())
    } else {
        Err(FirewallError::NotInstalled)
    }
}

fn helper_failure(status: ExitStatus, child: &mut Child) -> FirewallError {
    if matches!(
        status.code(),
        Some(PKEXEC_DISMISSED | PKEXEC_NOT_AUTHORIZED)
    ) {
        return FirewallError::NotAuthorized;
    }
    let mut message = String::new();
    if let Some(stderr) = child.stderr.take()
        && let Err(error) = stderr.take(MAX_ERROR_LENGTH).read_to_string(&mut message)
    {
        debug!(%error, "cannot read the helper's error message");
    }
    let message = message.trim();
    FirewallError::Helper(if message.is_empty() {
        status.to_string()
    } else {
        message.to_owned()
    })
}

/// Whether a firewall that this program knows how to open is running. If not, no
/// helper and no password are needed, unless the TV then fails to connect. Reading
/// this needs no privileges. Runs a command: do not call on the GTK thread.
pub fn is_active() -> bool {
    let ufw = fs::read_to_string(UFW_CONFIG).is_ok_and(|text| ufw_is_enabled(&text));
    ufw || firewalld_is_running()
}

fn ufw_is_enabled(config: &str) -> bool {
    config.lines().any(|line| {
        line.split_once('=').is_some_and(|(key, value)| {
            key.trim() == "ENABLED" && value.trim().eq_ignore_ascii_case("yes")
        })
    })
}

/// A missing `firewall-cmd` simply means firewalld is not in use.
fn firewalld_is_running() -> bool {
    Command::new(FIREWALL_CMD)
        .arg("--state")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Whether the helper has been installed, which opening the firewall needs.
pub fn helper_installed() -> bool {
    Path::new(HELPER_PATH).exists()
}

/// Whether a helper that is gone left firewall rules behind.
pub fn has_stale_rules() -> bool {
    match fs::read_to_string(MARKER_PATH) {
        Ok(text) => is_stale(&text),
        Err(_) => false,
    }
}

/// An unreadable marker also counts: only the helper can tell it is harmless.
fn is_stale(marker_text: &str) -> bool {
    Marker::parse(marker_text).is_none_or(|marker| !marker.helper_is_running())
}

/// Removes rules left by a helper that is gone. May ask for the password and
/// so block for a long time: do not call on the GTK thread.
pub fn restore_stale() -> Result<(), FirewallError> {
    ensure_helper_installed(Path::new(HELPER_PATH))?;
    run_to_end(PKEXEC, &[HELPER_PATH, "cleanup"])?;
    info!("removed firewall rules left by an earlier session");
    Ok(())
}

/// Asks for the administrator password now, so that opening the firewall later
/// does not keep the TV waiting. Polkit remembers the answer for a few minutes.
/// The cleanup command is harmless when there is nothing to clean up. May block
/// for a long time: do not call on the GTK thread.
pub fn authorize() -> Result<(), FirewallError> {
    ensure_helper_installed(Path::new(HELPER_PATH))?;
    run_to_end(PKEXEC, &[HELPER_PATH, "cleanup"])
}

fn run_to_end(program: &str, args: &[&str]) -> Result<(), FirewallError> {
    let mut child = helper_command(program, args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(helper_failure(status, &mut child))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHELL: &str = "/bin/sh";

    #[test]
    fn reads_whether_ufw_is_enabled() {
        assert!(ufw_is_enabled("# comment\nENABLED=yes\nLOGLEVEL=low\n"));
        assert!(ufw_is_enabled("ENABLED = YES"));
        for config in ["ENABLED=no", "#ENABLED=yes", "", "ENABLED", "OTHER=yes"] {
            assert!(!ufw_is_enabled(config), "{config:?}");
        }
    }

    /// A stand-in for the helper, as a shell script.
    fn fake_helper(script: &str) -> Result<FirewallSession, FirewallError> {
        FirewallSession::spawn(SHELL, &["-c", script])
    }

    #[test]
    fn open_succeeds_when_helper_is_ready() {
        let session = fake_helper("echo ready; cat > /dev/null").unwrap();
        session.close().unwrap();
    }

    #[test]
    fn close_tells_the_helper_to_finish_and_reports_its_failure() {
        let script = "echo ready; cat > /dev/null; echo rules remain >&2; exit 1";
        let error = fake_helper(script).unwrap().close().unwrap_err();
        assert!(matches!(error, FirewallError::Helper(message) if message == "rules remain"));
    }

    #[test]
    fn dropping_a_session_waits_for_the_helper() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("monitorbeam-drop-test-{}", std::process::id()));
        let script = format!(
            "echo ready; cat > /dev/null; echo done > '{}'",
            path.display()
        );
        drop(fake_helper(&script).unwrap());
        let written = fs::read_to_string(&path);
        let _ = fs::remove_file(&path);
        assert_eq!(written.unwrap(), "done\n");
    }

    #[test]
    fn dismissed_authorization_is_reported() {
        for code in [PKEXEC_DISMISSED, PKEXEC_NOT_AUTHORIZED] {
            let error = fake_helper(&format!("exit {code}")).unwrap_err();
            assert!(matches!(error, FirewallError::NotAuthorized), "{error}");
        }
    }

    #[test]
    fn helper_error_message_is_reported() {
        let error = fake_helper("echo no such interface >&2; exit 1").unwrap_err();
        assert!(
            matches!(&error, FirewallError::Helper(message) if message == "no such interface"),
            "{error}"
        );
    }

    #[test]
    fn helper_that_says_something_else_is_a_failure() {
        let error = fake_helper("echo not-ready; exit 3").unwrap_err();
        assert!(matches!(error, FirewallError::Helper(_)), "{error}");
    }

    #[test]
    fn endless_helper_output_is_not_trusted() {
        let error = fake_helper("yes ready | tr -d '\\n' | head -c 100000; exit 1").unwrap_err();
        assert!(matches!(error, FirewallError::Helper(_)), "{error}");
    }

    #[test]
    fn invalid_interface_never_reaches_the_helper() {
        for name in ["eth0", "-j", "p2p-*"] {
            let error = FirewallSession::open(name).unwrap_err();
            assert!(
                matches!(error, FirewallError::InvalidInterface(_)),
                "{error}"
            );
        }
    }

    #[test]
    fn missing_helper_is_reported_before_asking_for_a_password() {
        let error = ensure_helper_installed(Path::new("/nonexistent/helper")).unwrap_err();
        assert!(matches!(error, FirewallError::NotInstalled));
    }

    #[test]
    fn run_to_end_reports_success_and_failure() {
        assert!(run_to_end(SHELL, &["-c", "exit 0"]).is_ok());
        assert!(run_to_end(SHELL, &["-c", "echo bad >&2; exit 1"]).is_err());
    }

    #[test]
    fn marker_of_a_dead_helper_is_stale() {
        assert!(is_stale("4194999 p2p-wlo1-0 iptables\n"));
    }

    #[test]
    fn unreadable_marker_is_stale() {
        assert!(is_stale("garbage"));
    }

    #[test]
    fn other_process_with_the_recorded_id_is_stale() {
        // The recorded process exists, but it is this test, not the helper.
        assert!(is_stale(&format!(
            "{} p2p-wlo1-0 iptables\n",
            std::process::id()
        )));
    }
}
