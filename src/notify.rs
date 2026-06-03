//! Desktop notifications (replaces `notify-send`). Best-effort: failures fall
//! back to a log line so a headless box still works.

use tracing::info;

pub fn notify(title: &str, body: &str) {
    let shown = notify_rust::Notification::new()
        .summary(title)
        .body(body)
        .timeout(notify_rust::Timeout::Milliseconds(1500))
        .show();
    if shown.is_err() {
        info!("[{title}] {body}");
    }
}
