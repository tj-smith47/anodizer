use anyhow::{Context, Result};
use std::process::Command;

// ---------------------------------------------------------------------------
// Email parameters
// ---------------------------------------------------------------------------

/// Parameters needed to send an email notification.
pub struct EmailParams<'a> {
    pub from: &'a str,
    pub to: &'a [String],
    pub subject: &'a str,
    pub body: &'a str,
}

// ---------------------------------------------------------------------------
// Message builder (RFC 2822)
// ---------------------------------------------------------------------------

/// Build a minimal RFC 2822 message suitable for piping to sendmail/msmtp.
pub(crate) fn build_rfc2822_message(params: &EmailParams<'_>) -> String {
    let to_header = params.to.join(", ");
    format!(
        "From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nContent-Type: text/plain; charset=utf-8\r\nMIME-Version: 1.0\r\n\r\n{body}",
        from = params.from,
        to = to_header,
        subject = params.subject,
        body = params.body,
    )
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// Send an email by piping an RFC 2822 message to `sendmail` or `msmtp`.
///
/// Tries `sendmail -t` first; falls back to `msmtp -t` if sendmail is not
/// found. Both commands read recipients from the message headers via `-t`.
pub fn send_email(params: &EmailParams<'_>) -> Result<()> {
    let message = build_rfc2822_message(params);

    // Try sendmail first, then msmtp
    let (program, args) = if which_exists("sendmail") {
        ("sendmail", vec!["-t"])
    } else if which_exists("msmtp") {
        ("msmtp", vec!["-t"])
    } else {
        anyhow::bail!(
            "announce.email: neither `sendmail` nor `msmtp` found on PATH. \
             Install one to enable email announcements."
        );
    };

    let output = Command::new(program)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(message.as_bytes())?;
            }
            child.wait_with_output()
        })
        .with_context(|| format!("failed to run {program}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{program} exited with {}: {stderr}", output.status);
    }

    Ok(())
}

/// Check whether a program exists on PATH using the shared `find_binary` helper.
fn which_exists(program: &str) -> bool {
    anodize_core::util::find_binary(program)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_rfc2822_message_single_recipient() {
        let params = EmailParams {
            from: "release-bot@example.com",
            to: &["dev@example.com".to_string()],
            subject: "myapp v1.0.0 released",
            body: "A new version is available!",
        };
        let msg = build_rfc2822_message(&params);
        assert!(msg.contains("From: release-bot@example.com"));
        assert!(msg.contains("To: dev@example.com"));
        assert!(msg.contains("Subject: myapp v1.0.0 released"));
        assert!(msg.contains("Content-Type: text/plain; charset=utf-8"));
        assert!(msg.contains("A new version is available!"));
    }

    #[test]
    fn test_build_rfc2822_message_multiple_recipients() {
        let params = EmailParams {
            from: "bot@example.com",
            to: &[
                "alice@example.com".to_string(),
                "bob@example.com".to_string(),
            ],
            subject: "Release",
            body: "Done",
        };
        let msg = build_rfc2822_message(&params);
        assert!(msg.contains("To: alice@example.com, bob@example.com"));
    }

    #[test]
    fn test_rfc2822_header_body_separation() {
        let params = EmailParams {
            from: "a@b.com",
            to: &["c@d.com".to_string()],
            subject: "test",
            body: "body text here",
        };
        let msg = build_rfc2822_message(&params);
        // RFC 2822: headers and body separated by blank line (\r\n\r\n)
        assert!(msg.contains("\r\n\r\nbody text here"));
    }
}
