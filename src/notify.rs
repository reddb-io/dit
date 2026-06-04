//! Desktop notifications (replaces `notify-send`). Best-effort: failures fall
//! back to a log line so a headless box still works.
//!
//! Runs on a detached thread because the Linux backend (zbus) does a blocking
//! `block_on` internally — calling it directly from a Tokio worker panics with
//! "Cannot start a runtime from within a runtime".

use tracing::info;

pub fn notify(title: &str, body: &str) {
    let title = title.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        let shown = notify_rust::Notification::new()
            .summary(&title)
            .body(&body)
            .timeout(notify_rust::Timeout::Milliseconds(1500))
            .show();
        if shown.is_err() {
            info!("[{title}] {body}");
        }
    });
}
