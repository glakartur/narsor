//! `narsor-cli web`: opens the browser interface of the server.
//!
//! The interface is served by the API server itself under `/ui/`
//! (`narsor-web`). It needs the same bearer token as every other client, and
//! a trusted daemon makes a new one each time it starts. So this command
//! builds the address with the token in its fragment
//! ([`ApiClient::browser_url`](crate::api::ApiClient::browser_url)) and hands
//! it to the operator's browser. A browser does not send a fragment to any
//! server, and the interface removes it from the address bar at once.
//!
//! The address is a credential. It is printed only with `--no-open`, or when
//! no browser could be started, because then the operator has no other way
//! to get it. It is also never an argument of the opener: every user of the
//! machine can read the arguments of a process. The opener gets the path of
//! a page in the state directory, readable by the operator only, and that
//! page sends the browser on to the address.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::commands::Result;
use super::remote::{client, failure};
use crate::api::ServerTarget;
use crate::config::Flags;

/// The program that opens an address in the operator's browser.
const OPENER: &str = "xdg-open";

/// Opens the browser interface, or prints its address when `no_open`.
pub async fn open(no_open: bool, target: &ServerTarget, state: &Path, flags: &Flags) -> Result<()> {
    let client = client(target, state, flags).await?;
    let url = client
        .browser_url()
        .await
        .map_err(|error| failure("Could not read the token", error))?;
    if no_open {
        println!("{url}");
        return Ok(());
    }
    match write_launch_page(state, &url).and_then(|page| start_browser(&page)) {
        Ok(()) => println!("Opened {}/ui/ in the browser.", client.base_url()),
        Err(error) => {
            eprintln!(
                "Could not open a browser with {OPENER} ({error}). Open this address yourself:"
            );
            println!("{url}");
        }
    }
    Ok(())
}

/// The name of the launch page in the state directory.
const LAUNCH_PAGE: &str = "web-open.html";

/// Writes the page that sends a browser on to `url`, for the operator only.
///
/// The file is made afresh each time, so its mode is always the one set
/// here and a leftover from an earlier run never stays readable by others.
fn write_launch_page(state: &Path, url: &str) -> std::io::Result<PathBuf> {
    let path = state.join(LAUNCH_PAGE);
    match std::fs::remove_file(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(launch_page(url).as_bytes())?;
    Ok(path)
}

/// The launch page: one redirect, written so that the address cannot end
/// the attribute it stands in.
fn launch_page(url: &str) -> String {
    let address = url
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<!doctype html>\n<meta charset=\"utf-8\">\n<title>Narsor</title>\n\
         <meta http-equiv=\"refresh\" content=\"0; url={address}\">\n"
    )
}

/// Starts the opener on `page` and does not wait for the browser to close.
fn start_browser(page: &Path) -> std::io::Result<()> {
    Command::new(OPENER)
        .arg(page)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn the_launch_page_is_private_and_replaces_an_older_one() {
        let state = std::env::temp_dir().join(format!("narsor-web-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join(LAUNCH_PAGE), "old").unwrap();

        let page = write_launch_page(&state, "http://127.0.0.1:1/ui/#token=abc").unwrap();

        let mode = std::fs::metadata(&page).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let text = std::fs::read_to_string(&page).unwrap();
        assert!(
            text.contains("url=http://127.0.0.1:1/ui/#token=abc"),
            "{text}"
        );
        std::fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn the_launch_page_escapes_the_address() {
        let page = launch_page("https://x/ui/#token=a\"><script>");

        assert!(!page.contains("<script>"), "{page}");
        assert!(page.contains("&quot;&gt;&lt;script&gt;"), "{page}");
    }
}
