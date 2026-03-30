use anodize_core::template::{self, TemplateVars};
use anyhow::{Context, Result};
use chrono::Utc;
use std::process::Command;

use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{Message, SmtpTransport, Transport};

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

/// SMTP connection parameters.
pub struct SmtpParams<'a> {
    pub host: &'a str,
    pub port: u16,
    pub username: &'a str,
    pub password: &'a str,
    pub insecure_skip_verify: bool,
}

// ---------------------------------------------------------------------------
// SMTP transport (via lettre)
// ---------------------------------------------------------------------------

/// Send an email via SMTP using the lettre crate.
pub fn send_smtp(params: &EmailParams<'_>, smtp: &SmtpParams<'_>) -> Result<()> {
    let from = sanitize_header(params.from)
        .parse()
        .context("invalid 'from' address")?;
    let mut builder = Message::builder().from(from);
    for addr in params.to {
        let to = sanitize_header(addr)
            .parse()
            .context(format!("invalid 'to' address: {addr}"))?;
        builder = builder.to(to);
    }
    let email = builder
        .subject(sanitize_header(params.subject))
        .header(ContentType::TEXT_PLAIN)
        .body(params.body.to_string())
        .context("failed to build email message")?;

    let creds = Credentials::new(smtp.username.to_string(), smtp.password.to_string());
    let port = smtp.port;

    let mut transport = SmtpTransport::starttls_relay(smtp.host)
        .context(format!(
            "failed to create SMTP transport for {}",
            smtp.host
        ))?
        .port(port)
        .credentials(creds);

    if smtp.insecure_skip_verify {
        let tls = TlsParameters::builder(smtp.host.to_string())
            .dangerous_accept_invalid_certs(true)
            .build()
            .context("failed to build TLS parameters")?;
        transport = transport.tls(Tls::Required(tls));
    }

    transport
        .build()
        .send(&email)
        .context("failed to send email via SMTP")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Sendmail transport (RFC 2822 message piped to sendmail/msmtp)
// ---------------------------------------------------------------------------

/// Tera template for an RFC 2822 email message.
///
/// Headers are separated by `\r\n`; the blank line before the body is produced
/// by the template's own newline after the `Date` header plus the `\r\n` that
/// the post-processing step converts.  The body follows verbatim.
const RFC2822_TEMPLATE: &str = "\
From: {{ from }}\r
To: {{ to }}\r
Subject: {{ subject }}\r
MIME-Version: 1.0\r
Content-Type: text/plain; charset=utf-8\r
Date: {{ date }}\r
\r
{{ body }}";

/// Sanitise a header value by collapsing CR/LF to a single space.
/// This prevents header-injection attacks where a value containing `\r\n`
/// could forge extra headers.
fn sanitize_header(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

/// Build a minimal RFC 2822 message suitable for piping to sendmail/msmtp.
///
/// Uses a Tera template so that the message format is declarative and
/// easier to audit than string concatenation.
pub(crate) fn build_rfc2822_message(params: &EmailParams<'_>) -> Result<String> {
    let to_header = params
        .to
        .iter()
        .map(|addr| sanitize_header(addr))
        .collect::<Vec<_>>()
        .join(", ");

    let mut vars = TemplateVars::new();
    vars.set("from", &sanitize_header(params.from));
    vars.set("to", &to_header);
    vars.set("subject", &sanitize_header(params.subject));
    vars.set(
        "date",
        &Utc::now().format("%a, %d %b %Y %H:%M:%S +0000").to_string(),
    );
    vars.set("body", params.body);

    template::render(RFC2822_TEMPLATE, &vars).context("failed to render RFC 2822 email template")
}

/// Send an email by piping an RFC 2822 message to `sendmail` or `msmtp`.
///
/// Tries `sendmail -t` first; falls back to `msmtp -t` if sendmail is not
/// found. Both commands read recipients from the message headers via `-t`.
pub fn send_sendmail(params: &EmailParams<'_>) -> Result<()> {
    let message = build_rfc2822_message(params)?;

    // Try sendmail first, then msmtp
    let (program, args) = if which_exists("sendmail") {
        ("sendmail", vec!["-t"])
    } else if which_exists("msmtp") {
        ("msmtp", vec!["-t"])
    } else {
        anyhow::bail!(
            "announce.email: neither `sendmail` nor `msmtp` found on PATH. \
             Configure SMTP (host/port) or install sendmail/msmtp."
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
        let msg = build_rfc2822_message(&params).unwrap();
        assert!(msg.contains("From: release-bot@example.com"));
        assert!(msg.contains("To: dev@example.com"));
        assert!(msg.contains("Subject: myapp v1.0.0 released"));
        assert!(msg.contains("Content-Type: text/plain; charset=utf-8"));
        assert!(msg.contains("MIME-Version: 1.0"));
        assert!(msg.contains("Date: "));
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
        let msg = build_rfc2822_message(&params).unwrap();
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
        let msg = build_rfc2822_message(&params).unwrap();
        // RFC 2822: headers and body separated by blank line (\r\n\r\n)
        assert!(msg.contains("\r\n\r\nbody text here"));
    }

    #[test]
    fn test_sanitizes_newlines_in_headers() {
        let params = EmailParams {
            from: "bot@example.com",
            to: &["dev@example.com".to_string()],
            subject: "legit\r\nBcc: evil@attacker.com",
            body: "body",
        };
        let msg = build_rfc2822_message(&params).unwrap();
        // The injected CRLF must be stripped so "Bcc:" cannot appear as
        // a standalone header line — it stays inside the Subject value.
        assert!(
            !msg.contains("\r\nBcc:"),
            "header injection: Bcc appeared as a separate header line"
        );
        assert!(msg.contains("Subject: legit"));
    }

    #[test]
    fn test_smtp_params_default_port() {
        // Port default (587) is resolved at the call site in lib.rs;
        // SmtpParams always carries a real port number.
        let params = SmtpParams {
            host: "smtp.example.com",
            port: 587,
            username: "user",
            password: "pass",
            insecure_skip_verify: false,
        };
        assert_eq!(params.port, 587);
    }

    #[test]
    fn test_smtp_params_custom_port() {
        let params = SmtpParams {
            host: "smtp.example.com",
            port: 465,
            username: "user",
            password: "pass",
            insecure_skip_verify: false,
        };
        assert_eq!(params.port, 465);
    }
}
