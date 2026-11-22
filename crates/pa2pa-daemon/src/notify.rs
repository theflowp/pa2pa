use tokio::process::Command;

fn sanitize_notification_text(raw: &str, max_chars: usize) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        format!("{}…", trimmed.chars().take(max_chars).collect::<String>())
    }
}

/// Best-effort native OS desktop notification (macOS via osascript argv, Linux via notify-send --).
/// Passes untrusted peer text strictly via process argv — zero script or flag injection.
pub async fn send_desktop_notification(title: &str, subtitle: &str, body: &str) {
    let safe_title = sanitize_notification_text(title, 80);
    let safe_subtitle = sanitize_notification_text(subtitle, 100);
    let safe_body = sanitize_notification_text(body, 240);

    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("osascript")
            .arg("-e")
            .arg("on run argv")
            .arg("-e")
            .arg("display notification (item 1 of argv) with title (item 2 of argv) subtitle (item 3 of argv)")
            .arg("-e")
            .arg("end run")
            .arg("--")
            .arg(&safe_body)
            .arg(&safe_title)
            .arg(&safe_subtitle)
            .output()
            .await;
    }

    #[cfg(target_os = "linux")]
    {
        let full_title = if safe_subtitle.is_empty() {
            safe_title
        } else {
            format!("{safe_title} — {safe_subtitle}")
        };
        let _ = Command::new("notify-send")
            .arg("--")
            .arg(full_title)
            .arg(safe_body)
            .output()
            .await;
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (safe_title, safe_subtitle, safe_body);
    }
}
