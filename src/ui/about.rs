//! What the program says about itself, in the splash screen and the About dialog.

use adw::prelude::*;

use super::APP_ID;

pub const NAME: &str = "MonitorBeam";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SUMMARY: &str = "Mirror one monitor to your TV over Miracast";
pub const COPYRIGHT: &str = "© 2026 Matej Jazbinšek";
const WEBSITE: &str = "https://github.com/sinkec/MonitorBeam";
const ISSUES: &str = "https://github.com/sinkec/MonitorBeam/issues";
pub const LICENSE_NOTICE: &str = "Free software under the GNU GPL, version 3 or later.\n\
                                  It comes with absolutely no warranty.";
pub const PRIVACY_NOTICE: &str =
    "No telemetry. It connects only to your TV, and captures the screen only when you ask.";

/// Versions that matter in a bug report, from the libraries actually loaded.
fn library_versions() -> String {
    format!(
        "MonitorBeam {VERSION}\nGTK {}.{}.{}\nlibadwaita {}.{}.{}\n{}\n",
        gtk::major_version(),
        gtk::minor_version(),
        gtk::micro_version(),
        adw::major_version(),
        adw::minor_version(),
        adw::micro_version(),
        gst::version_string(),
    )
}

pub fn show_dialog(parent: &impl IsA<gtk::Widget>) {
    let dialog = adw::AboutDialog::builder()
        .application_name(NAME)
        .application_icon(APP_ID)
        .version(VERSION)
        .comments(format!("{SUMMARY}.\n\n{PRIVACY_NOTICE}"))
        .developer_name("Matej Jazbinšek")
        .website(WEBSITE)
        .issue_url(ISSUES)
        .copyright(COPYRIGHT)
        .license_type(gtk::License::Gpl30)
        .debug_info(library_versions())
        .debug_info_filename("monitorbeam-versions.txt")
        .build();
    dialog.present(Some(parent));
}
