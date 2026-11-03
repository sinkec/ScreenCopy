//! Privileged helper that opens the firewall for one Miracast session.
//!
//! Runs as root through `pkexec`. It can do exactly one thing: allow incoming
//! RTSP and DHCP on one Wi-Fi Direct interface. It takes no paths, ports or
//! rules from its caller, and never disables the firewall.
//!
//!     open <interface>  opens, prints "ready", waits for stdin to close, closes
//!     cleanup           closes what a helper that was killed left open
//!
//! The caller holds stdin open for the session, so the operating system closes
//! it however the caller ends, including a crash. With ufw the rules go into
//! the running ruleset only, so they also cannot survive a reboot.

#[path = "../firewall/protocol.rs"]
mod protocol;

use std::fs;
use std::io::{self, Read, Write};
use std::process::{Command, ExitCode, Stdio};

use protocol::{MARKER_PATH, Marker, is_valid_p2p_interface};

const SAFE_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";
/// RTSP control connection from the TV.
const WFD_RTSP_PORT: &str = "7236";
/// DHCP requests from the TV while this computer is the group owner.
const DHCP_SERVER_PORT: &str = "67";
const IPTABLES_RULES: [(&str, &str); 2] = [("tcp", WFD_RTSP_PORT), ("udp", DHCP_SERVER_PORT)];
const RULE_COMMENT: &str = "monitorbeam";
/// Seconds to wait for the iptables lock, so that closing cannot hang forever.
const IPTABLES_WAIT: &str = "5";
/// Removal repeats in case an earlier session left duplicates behind.
const MAX_DUPLICATE_RULES: usize = 16;
/// Installed from `data/monitorbeam.firewalld.xml`.
const FIREWALLD_ZONE: &str = "monitorbeam";

const BACKEND_FIREWALLD: &str = "firewalld";
const BACKEND_IPTABLES: &str = "iptables";

/// Runs one external command and reports whether it succeeded.
trait Runner {
    fn run(&mut self, program: &str, args: &[String]) -> io::Result<bool>;
}

struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&mut self, program: &str, args: &[String]) -> io::Result<bool> {
        let status = Command::new(program)
            .args(args)
            .env_clear()
            .env("PATH", SAFE_PATH)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        Ok(status.success())
    }
}

fn iptables_args(action: &str, interface: &str, protocol: &str, port: &str) -> Vec<String> {
    [
        "-w",
        IPTABLES_WAIT,
        action,
        "INPUT",
        "-i",
        interface,
        "-p",
        protocol,
        "--dport",
        port,
        "-m",
        "comment",
        "--comment",
        RULE_COMMENT,
        "-j",
        "ACCEPT",
    ]
    .map(str::to_owned)
    .to_vec()
}

fn firewalld_args(option: &str, interface: &str) -> Vec<String> {
    vec![
        format!("--zone={FIREWALLD_ZONE}"),
        format!("--{option}={interface}"),
    ]
}

fn detect_backend(runner: &mut impl Runner) -> &'static str {
    // A missing firewall-cmd simply means firewalld is not in use.
    match runner.run("firewall-cmd", &["--state".to_owned()]) {
        Ok(true) => BACKEND_FIREWALLD,
        _ => BACKEND_IPTABLES,
    }
}

fn open_rules(runner: &mut impl Runner, backend: &str, interface: &str) -> Result<(), String> {
    if backend == BACKEND_FIREWALLD {
        let args = firewalld_args("change-interface", interface);
        return match runner.run("firewall-cmd", &args) {
            Ok(true) => Ok(()),
            Ok(false) => Err(format!(
                "firewalld refused to use the {FIREWALLD_ZONE} zone; is it installed?"
            )),
            Err(error) => Err(format!("cannot run firewall-cmd: {error}")),
        };
    }
    for (protocol, port) in IPTABLES_RULES {
        let args = iptables_args("-I", interface, protocol, port);
        match runner.run("iptables", &args) {
            Ok(true) => {}
            Ok(false) => return Err(format!("iptables refused the rule for {protocol}/{port}")),
            Err(error) => return Err(format!("cannot run iptables: {error}")),
        }
    }
    Ok(())
}

/// Removes everything `open_rules` may have added. Rules that are already gone
/// are not an error; only an unusable tool is.
fn close_rules(runner: &mut impl Runner, backend: &str, interface: &str) -> Result<(), String> {
    if backend == BACKEND_FIREWALLD {
        let args = firewalld_args("remove-interface", interface);
        return match runner.run("firewall-cmd", &args) {
            Ok(_) => Ok(()),
            Err(error) => Err(format!("cannot run firewall-cmd: {error}")),
        };
    }
    for (protocol, port) in IPTABLES_RULES {
        let args = iptables_args("-D", interface, protocol, port);
        for _ in 0..MAX_DUPLICATE_RULES {
            match runner.run("iptables", &args) {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => return Err(format!("cannot run iptables: {error}")),
            }
        }
    }
    Ok(())
}

fn read_marker() -> Option<Marker> {
    Marker::parse(&fs::read_to_string(MARKER_PATH).ok()?)
}

/// Blocks until the caller goes away, for whatever reason.
fn wait_for_caller_to_end(input: &mut impl Read) {
    let mut buffer = [0; 64];
    while matches!(input.read(&mut buffer), Ok(count) if count > 0) {}
}

/// Closes and forgets the session. The marker stays on failure, so that the
/// next start of the program tries again.
fn finish(runner: &mut impl Runner, backend: &str, interface: &str) -> Result<(), String> {
    close_rules(runner, backend, interface)?;
    remove_marker()
}

fn remove_marker() -> Result<(), String> {
    match fs::remove_file(MARKER_PATH) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(format!("cannot remove {MARKER_PATH}: {error}"))
        }
        _ => Ok(()),
    }
}

fn command_open(interface: &str) -> Result<(), String> {
    if !is_valid_p2p_interface(interface) {
        return Err("not a Wi-Fi Direct interface name".to_owned());
    }
    if !fs::exists(format!("/sys/class/net/{interface}")).unwrap_or(false) {
        return Err(format!("interface {interface} does not exist"));
    }
    if let Some(marker) = read_marker() {
        if marker.helper_is_running() {
            return Err("another session is already active".to_owned());
        }
        finish(&mut SystemRunner, &marker.backend, &marker.interface)?;
    }

    let mut runner = SystemRunner;
    let backend = detect_backend(&mut runner);
    let marker = Marker {
        pid: std::process::id(),
        interface: interface.to_owned(),
        backend: backend.to_owned(),
    };
    // Written first, so that being killed at any later point stays recoverable.
    fs::write(MARKER_PATH, marker.format())
        .map_err(|error| format!("cannot write {MARKER_PATH}: {error}"))?;

    if let Err(error) = open_rules(&mut runner, backend, interface) {
        if let Err(cleanup_error) = finish(&mut runner, backend, interface) {
            return Err(format!("{error}; cleaning up also failed: {cleanup_error}"));
        }
        return Err(error);
    }

    let mut output = io::stdout();
    let announced = writeln!(output, "ready").and_then(|()| output.flush());
    if announced.is_ok() {
        wait_for_caller_to_end(&mut io::stdin());
    }
    finish(&mut runner, backend, interface)
}

fn command_cleanup() -> Result<(), String> {
    let Some(marker) = read_marker() else {
        return remove_marker();
    };
    if marker.helper_is_running() {
        return Err("the session is still active".to_owned());
    }
    finish(&mut SystemRunner, &marker.backend, &marker.interface)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["open", interface] => command_open(interface),
        ["cleanup"] => command_cleanup(),
        _ => Err("usage: open <interface> | cleanup".to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records commands. `results` answers them in order, then `default` applies.
    struct FakeRunner {
        calls: Vec<String>,
        results: Vec<io::Result<bool>>,
        default: bool,
    }

    impl FakeRunner {
        fn new(default: bool, results: Vec<io::Result<bool>>) -> Self {
            Self {
                calls: Vec::new(),
                results,
                default,
            }
        }
    }

    impl Runner for FakeRunner {
        fn run(&mut self, program: &str, args: &[String]) -> io::Result<bool> {
            self.calls.push(format!("{program} {}", args.join(" ")));
            if self.results.is_empty() {
                Ok(self.default)
            } else {
                self.results.remove(0)
            }
        }
    }

    fn missing() -> io::Result<bool> {
        Err(io::Error::from(io::ErrorKind::NotFound))
    }

    const ADD_TCP: &str = "iptables -w 5 -I INPUT -i p2p-wlo1-0 -p tcp --dport 7236 \
                           -m comment --comment monitorbeam -j ACCEPT";
    const ADD_UDP: &str = "iptables -w 5 -I INPUT -i p2p-wlo1-0 -p udp --dport 67 \
                           -m comment --comment monitorbeam -j ACCEPT";
    const DEL_TCP: &str = "iptables -w 5 -D INPUT -i p2p-wlo1-0 -p tcp --dport 7236 \
                           -m comment --comment monitorbeam -j ACCEPT";
    const DEL_UDP: &str = "iptables -w 5 -D INPUT -i p2p-wlo1-0 -p udp --dport 67 \
                           -m comment --comment monitorbeam -j ACCEPT";

    #[test]
    fn opens_only_the_two_rules_on_the_interface() {
        let mut runner = FakeRunner::new(true, vec![]);
        open_rules(&mut runner, BACKEND_IPTABLES, "p2p-wlo1-0").unwrap();
        assert_eq!(runner.calls, [ADD_TCP, ADD_UDP]);
    }

    #[test]
    fn open_reports_a_refused_rule() {
        let mut runner = FakeRunner::new(true, vec![Ok(true), Ok(false)]);
        let error = open_rules(&mut runner, BACKEND_IPTABLES, "p2p-wlo1-0").unwrap_err();
        assert!(error.contains("udp/67"), "{error}");
    }

    #[test]
    fn close_removes_rules_and_duplicates_until_none_remain() {
        let mut runner = FakeRunner::new(false, vec![Ok(true), Ok(true), Ok(false), Ok(true)]);
        close_rules(&mut runner, BACKEND_IPTABLES, "p2p-wlo1-0").unwrap();
        assert_eq!(runner.calls, [DEL_TCP, DEL_TCP, DEL_TCP, DEL_UDP, DEL_UDP]);
    }

    #[test]
    fn close_accepts_rules_that_are_already_gone() {
        let mut runner = FakeRunner::new(false, vec![]);
        assert!(close_rules(&mut runner, BACKEND_IPTABLES, "p2p-wlo1-0").is_ok());
    }

    #[test]
    fn close_stops_at_the_duplicate_limit() {
        let mut runner = FakeRunner::new(true, vec![]);
        close_rules(&mut runner, BACKEND_IPTABLES, "p2p-wlo1-0").unwrap();
        assert_eq!(
            runner.calls.len(),
            MAX_DUPLICATE_RULES * IPTABLES_RULES.len()
        );
    }

    #[test]
    fn close_reports_a_missing_tool() {
        let mut runner = FakeRunner::new(true, vec![missing()]);
        assert!(close_rules(&mut runner, BACKEND_IPTABLES, "p2p-wlo1-0").is_err());
    }

    #[test]
    fn firewalld_moves_the_interface_to_its_own_zone_and_back() {
        let mut runner = FakeRunner::new(true, vec![]);
        open_rules(&mut runner, BACKEND_FIREWALLD, "p2p-wlo1-0").unwrap();
        close_rules(&mut runner, BACKEND_FIREWALLD, "p2p-wlo1-0").unwrap();
        assert_eq!(
            runner.calls,
            [
                "firewall-cmd --zone=monitorbeam --change-interface=p2p-wlo1-0",
                "firewall-cmd --zone=monitorbeam --remove-interface=p2p-wlo1-0",
            ]
        );
    }

    #[test]
    fn firewalld_is_used_only_when_it_runs() {
        let mut running = FakeRunner::new(true, vec![]);
        assert_eq!(detect_backend(&mut running), BACKEND_FIREWALLD);
        let mut stopped = FakeRunner::new(false, vec![]);
        assert_eq!(detect_backend(&mut stopped), BACKEND_IPTABLES);
        let mut absent = FakeRunner::new(true, vec![missing()]);
        assert_eq!(detect_backend(&mut absent), BACKEND_IPTABLES);
    }

    #[test]
    fn waits_until_input_ends() {
        let mut input: &[u8] = b"anything at all";
        wait_for_caller_to_end(&mut input);
        assert!(input.is_empty());
    }

    #[test]
    fn accepts_p2p_interface_names() {
        for name in ["p2p-wlo1-0", "p2p-test0", "p2p-wlp3s0-12", "p2p-a_b"] {
            assert!(is_valid_p2p_interface(name), "{name}");
        }
    }

    #[test]
    fn rejects_other_interface_names() {
        for name in [
            "",
            "p2p-",
            "wlo1",
            "eth0",
            "p2p+",
            "p2p-+",
            "p2p-*",
            "-j",
            "--dport",
            "p2p-a b",
            "p2p-a;b",
            "p2p-../x",
            "p2p-a\n",
            "p2p-0123456789ab",
            "P2P-wlo1",
            "p2p-č",
        ] {
            assert!(!is_valid_p2p_interface(name), "{name:?}");
        }
    }

    #[test]
    fn marker_round_trips() {
        let marker = Marker {
            pid: 42,
            interface: "p2p-wlo1-0".to_owned(),
            backend: BACKEND_IPTABLES.to_owned(),
        };
        assert_eq!(Marker::parse(&marker.format()), Some(marker));
    }

    #[test]
    fn rejects_malformed_markers() {
        for text in [
            "",
            "42",
            "42 p2p-wlo1-0",
            "x p2p-wlo1-0 iptables",
            "-1 p2p-wlo1-0 iptables",
            "42 eth0 iptables",
            "42 p2p-wlo1-0 iptables extra",
        ] {
            assert_eq!(Marker::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn process_that_is_not_the_helper_does_not_count_as_running() {
        let marker = Marker {
            pid: std::process::id(),
            interface: "p2p-wlo1-0".to_owned(),
            backend: BACKEND_IPTABLES.to_owned(),
        };
        assert!(!marker.helper_is_running());
    }
}
