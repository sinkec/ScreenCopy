//! Definitions shared by the program and its privileged firewall helper.
//!
//! The helper includes this file directly, so it stays free of the program's
//! dependencies. It must only use the standard library.

use std::fs;

/// Written and removed only by the helper, so it cannot be forged by a user.
/// `/run` is emptied at boot, exactly like the rules the file describes.
pub const MARKER_PATH: &str = "/run/screencopy-firewall";

/// The kernel limits process names to 15 bytes.
const HELPER_PROCESS_NAME: &str = "screencopy-fire";

/// Longest interface name the kernel accepts (`IFNAMSIZ` without the NUL).
const MAX_INTERFACE_LENGTH: usize = 15;
const MAX_MARKER_FIELDS: usize = 3;

/// Accepts only Wi-Fi Direct interface names, such as `p2p-wlo1-0`. This also
/// keeps out wildcards and anything `iptables` could read as an option.
pub fn is_valid_p2p_interface(name: &str) -> bool {
    name.len() <= MAX_INTERFACE_LENGTH
        && name.strip_prefix("p2p-").is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
}

/// What the helper has opened, and which helper process is responsible for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub pid: u32,
    pub interface: String,
    pub backend: String,
}

impl Marker {
    pub fn parse(text: &str) -> Option<Self> {
        let fields: Vec<&str> = text
            .split_whitespace()
            .take(MAX_MARKER_FIELDS + 1)
            .collect();
        let [pid, interface, backend] = fields.as_slice() else {
            return None;
        };
        if !is_valid_p2p_interface(interface) {
            return None;
        }
        Some(Self {
            pid: pid.parse().ok()?,
            interface: (*interface).to_owned(),
            backend: (*backend).to_owned(),
        })
    }

    pub fn format(&self) -> String {
        format!("{} {} {}\n", self.pid, self.interface, self.backend)
    }

    /// Whether the responsible helper still runs. The process name guards
    /// against the process ID having been reused by another program.
    pub fn helper_is_running(&self) -> bool {
        fs::read_to_string(format!("/proc/{}/comm", self.pid))
            .is_ok_and(|name| name.trim_end() == HELPER_PROCESS_NAME)
    }
}
