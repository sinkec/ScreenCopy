//! Session-scoped firewall rules for the Wi-Fi Direct interface.
//!
//! All privileged work happens in `monitorbeam-firewall-helper`, started once through
//! `pkexec`, so that the administrator password is needed once while the program runs.
//! The helper opens the firewall only for each session. It removes the rules when the
//! pipe to it closes, so a crash of this program cannot leave them behind. Only the
//! helper being killed can, and then `restore_stale` finds them through its marker file.

pub mod protocol;

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Mutex, PoisonError};

use thiserror::Error;
use tracing::{debug, info, warn};

use protocol::{MARKER_PATH, Marker, is_valid_p2p_interface};

const PKEXEC: &str = "/usr/bin/pkexec";
/// Fixed and root-owned: the polkit policy authorizes exactly this path.
const HELPER_PATH: &str = "/usr/libexec/monitorbeam/monitorbeam-firewall-helper";
const READY: &str = "ready";
const CLOSED: &str = "closed";
const ERROR_PREFIX: &str = "error ";
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
    /// The helper is fine, but will not do what was asked.
    #[error("the firewall helper refused: {0}")]
    Refused(String),
}

/// The running helper. It stays until the program ends, when the pipe closes.
static HELPER: Mutex<Option<Helper>> = Mutex::new(None);

/// The waiting helper, and the way to talk to it.
#[derive(Debug)]
struct Helper {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Helper {
    /// Starts the helper, which may ask for the password, and waits until it is ready.
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
        let mut stdout = BufReader::new(stdout);
        if matches!(read_answer(&mut stdout), Ok(answer) if answer == READY) {
            return Ok(Self {
                child,
                stdin,
                stdout,
            });
        }
        // Without its pipes, a helper that is still writing stops instead of blocking.
        drop(stdin);
        drop(stdout);
        let status = child.wait()?;
        Err(helper_failure(status, &mut child))
    }

    /// Sends a command, and returns when the helper has carried it out.
    fn command(&mut self, command: &str, success: &str) -> Result<(), FirewallError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| FirewallError::Helper("the helper has stopped".to_owned()))?;
        writeln!(stdin, "{command}")?;
        stdin.flush()?;
        let answer = read_answer(&mut self.stdout)?;
        if answer == success {
            return Ok(());
        }
        let reason = answer.strip_prefix(ERROR_PREFIX).unwrap_or(&answer);
        Err(FirewallError::Refused(reason.to_owned()))
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        // Closing the pipe is what tells the helper to finish.
        drop(self.stdin.take());
        if let Err(error) = self.child.wait() {
            warn!(%error, "cannot wait for the firewall helper");
        }
    }
}

/// One line from the helper, which is not trusted to end it.
fn read_answer(stdout: &mut BufReader<ChildStdout>) -> Result<String, FirewallError> {
    let mut line = String::new();
    stdout.by_ref().take(MAX_LINE_LENGTH).read_line(&mut line)?;
    if !line.ends_with('\n') {
        return Err(FirewallError::Helper("the helper has stopped".to_owned()));
    }
    Ok(line.trim_end().to_owned())
}

fn the_helper() -> std::sync::MutexGuard<'static, Option<Helper>> {
    // A panic elsewhere cannot make the helper unusable.
    HELPER.lock().unwrap_or_else(PoisonError::into_inner)
}

fn start_helper() -> Result<Helper, FirewallError> {
    ensure_helper_installed(Path::new(HELPER_PATH))?;
    Helper::spawn(PKEXEC, &[HELPER_PATH, "serve"])
}

/// Runs `command` on the helper, starting it if it does not run yet. If it was killed
/// since, it starts again, which asks for the password again.
fn with_helper(command: &str, success: &str) -> Result<(), FirewallError> {
    let mut slot = the_helper();
    let mut helper = match slot.take() {
        Some(helper) => helper,
        None => start_helper()?,
    };
    let mut answer = helper.command(command, success);
    // A refusal comes from a working helper. Anything else means that it has gone.
    if answer
        .as_ref()
        .is_err_and(|error| !matches!(error, FirewallError::Refused(_)))
    {
        drop(helper);
        helper = start_helper()?;
        answer = helper.command(command, success);
    }
    *slot = Some(helper);
    answer
}

/// An open firewall for one session. Rules last until `close` or drop.
#[derive(Debug)]
pub struct FirewallSession {
    open: bool,
}

impl FirewallSession {
    /// Opens the firewall on `interface`. May ask for the administrator
    /// password and so block for a long time: do not call on the GTK thread.
    pub fn open(interface: &str) -> Result<Self, FirewallError> {
        if !is_valid_p2p_interface(interface) {
            return Err(FirewallError::InvalidInterface(interface.to_owned()));
        }
        with_helper(&format!("open {interface}"), READY)?;
        info!(interface, "firewall opened for session");
        Ok(Self { open: true })
    }

    /// Removes the rules and reports whether that worked.
    pub fn close(mut self) -> Result<(), FirewallError> {
        self.end()
    }

    fn end(&mut self) -> Result<(), FirewallError> {
        if !self.open {
            return Ok(());
        }
        self.open = false;
        // A helper that is gone has removed the rules, or left them for
        // `restore_stale`. Starting it again here would only ask for a password.
        if let Some(helper) = the_helper().as_mut() {
            helper.command("close", CLOSED)?;
        }
        debug!("firewall closed");
        Ok(())
    }
}

impl Drop for FirewallSession {
    fn drop(&mut self) {
        if let Err(error) = self.end() {
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

/// Starts the helper now if it does not run yet, which asks for the administrator
/// password, so that opening the firewall later does not keep the TV waiting. It
/// needs asking only once while the program runs. May block for a long time: do not
/// call on the GTK thread.
pub fn authorize() -> Result<(), FirewallError> {
    let mut slot = the_helper();
    if slot.is_none() {
        *slot = Some(start_helper()?);
    }
    Ok(())
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
    fn fake_helper(script: &str) -> Result<Helper, FirewallError> {
        Helper::spawn(SHELL, &["-c", script])
    }

    #[test]
    fn the_helper_carries_out_several_sessions_without_starting_again() {
        let script = "echo ready; while read command; do \
                      case \"$command\" in open*) echo ready;; close) echo closed;; esac; done";
        let mut helper = fake_helper(script).unwrap();
        for _ in 0..3 {
            helper.command("open p2p-test0", READY).unwrap();
            helper.command("close", CLOSED).unwrap();
        }
    }

    #[test]
    fn a_refusal_gives_its_reason() {
        let script = "echo ready; read command; echo error no such interface; cat > /dev/null";
        let mut helper = fake_helper(script).unwrap();
        let error = helper.command("open p2p-test0", READY).unwrap_err();
        assert!(
            matches!(&error, FirewallError::Refused(reason) if reason == "no such interface"),
            "{error}"
        );
    }

    #[test]
    fn a_helper_that_has_gone_is_noticed() {
        let mut helper = fake_helper("echo ready; exit 0").unwrap();
        let error = helper.command("open p2p-test0", READY).unwrap_err();
        assert!(!matches!(error, FirewallError::Refused(_)), "{error}");
    }

    #[test]
    fn dropping_the_helper_tells_it_to_finish_and_waits() {
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
