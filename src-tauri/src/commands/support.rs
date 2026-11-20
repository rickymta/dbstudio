//! "Help & Support" form: sends a support request as an email through FormSubmit
//! (formsubmit.co), a form-to-email relay that needs no account or API key. The request goes from
//! Rust, so the packaged app works without a mail client and without widening the webview CSP.
//!
//! Only what the user typed in the form is sent, plus the app version and OS when they leave
//! that box ticked. Nothing about connections, statements or data is attached.
//!
//! The first message to a new address makes FormSubmit send that address a one-time
//! "Activate form" email; messages sent before activation are held until it is confirmed.

use std::time::Duration;

use dbstudio_core::{Error, Result};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tauri::State;

use super::app::{AppInfo, app_info_impl};
use crate::state::AppState;

/// Where support requests are delivered.
pub const SUPPORT_EMAIL: &str = "rickymta@gmail.com";
const ENDPOINT: &str = "https://formsubmit.co/ajax/";
/// Sent as the form's page (`_url`, and the Referer header).
const PROJECT_URL: &str = "https://github.com/rickymta/dbstudio";

/// Longest accepted name, subject and message (characters). Mirrored by the form in the UI.
pub const MAX_NAME: usize = 100;
pub const MAX_SUBJECT: usize = 200;
pub const MAX_MESSAGE: usize = 10_000;

/// What the Help form sends.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupportRequest {
    pub name: String,
    pub email: String,
    /// `bug` | `feature` | `question` | `other`.
    pub topic: String,
    pub subject: String,
    pub message: String,
    /// Attach the app version and OS to the email.
    #[serde(default)]
    pub include_system: bool,
}

fn topic_label(topic: &str) -> Option<&'static str> {
    Some(match topic {
        "bug" => "Bug report",
        "feature" => "Feature request",
        "question" => "Question",
        "other" => "Other",
        _ => return None,
    })
}

/// Loose `local@domain.tld` check, mirrored by the form in the UI.
pub fn valid_email(s: &str) -> bool {
    let s = s.trim();
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !s.chars().any(char::is_whitespace)
        && !domain.contains('@')
        && domain
            .rsplit_once('.')
            .is_some_and(|(host, tld)| !host.is_empty() && !tld.is_empty())
}

impl SupportRequest {
    /// Reject empty, malformed or oversized fields.
    pub fn validate(&self) -> Result<()> {
        let required = [
            ("name", &self.name),
            ("email", &self.email),
            ("subject", &self.subject),
            ("message", &self.message),
        ];
        if let Some((field, _)) = required.iter().find(|(_, v)| v.trim().is_empty()) {
            return Err(Error::Invalid(format!("the {field} field is required")));
        }
        if !valid_email(&self.email) {
            return Err(Error::Invalid("the email address is not valid".into()));
        }
        if topic_label(&self.topic).is_none() {
            return Err(Error::Invalid(format!("unknown topic `{}`", self.topic)));
        }
        let too_long = |v: &str, max: usize| v.trim().chars().count() > max;
        if too_long(&self.name, MAX_NAME)
            || too_long(&self.subject, MAX_SUBJECT)
            || too_long(&self.message, MAX_MESSAGE)
        {
            return Err(Error::Invalid("a field is too long".into()));
        }
        Ok(())
    }

    /// FormSubmit fields. Keys starting with `_` configure the email; the rest become its rows.
    pub fn payload(&self, app: &AppInfo) -> Value {
        let topic = topic_label(&self.topic).unwrap_or("Other");
        let mut m = Map::new();
        m.insert("Name".into(), json!(self.name.trim()));
        m.insert("Email".into(), json!(self.email.trim()));
        m.insert("Topic".into(), json!(topic));
        m.insert("Subject".into(), json!(self.subject.trim()));
        m.insert("Message".into(), json!(self.message.trim()));
        if self.include_system {
            m.insert("App version".into(), json!(app.version));
            m.insert("OS".into(), json!(format!("{} ({})", app.os, app.arch)));
        }
        m.insert(
            "_subject".into(),
            json!(format!("[DBStudio] {topic}: {}", self.subject.trim())),
        );
        m.insert("_replyto".into(), json!(self.email.trim()));
        m.insert("_template".into(), json!("table"));
        m.insert("_captcha".into(), json!("false"));
        // FormSubmit refuses requests that do not come from a web page unless `_url` names the
        // form's page.
        m.insert("_url".into(), json!(PROJECT_URL));
        Value::Object(m)
    }
}

/// FormSubmit answers `{ "success": "true" | "false", "message": "..." }` (sometimes booleans).
pub(crate) fn check_response(status: u16, body: &str) -> Result<()> {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let ok = match v.get("success") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    };
    if (200..300).contains(&status) && ok {
        return Ok(());
    }
    let msg = v
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("HTTP {status}"));
    Err(Error::Connection(format!(
        "the support form was not sent: {msg}"
    )))
}

pub(crate) async fn support_send_impl(state: &AppState, request: &SupportRequest) -> Result<()> {
    request.validate()?;
    let app = app_info_impl(state);
    // reqwest is built without a TLS crypto provider; the workspace uses ring.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(format!("DBStudio/{}", app.version))
        .build()
        .map_err(|e| Error::Connection(e.to_string()))?;
    let res = client
        .post(format!("{ENDPOINT}{SUPPORT_EMAIL}"))
        .header("Accept", "application/json")
        .header("Referer", PROJECT_URL)
        .json(&request.payload(&app))
        .send()
        .await
        .map_err(|e| Error::Connection(format!("could not reach the support service: {e}")))?;
    let status = res.status().as_u16();
    let body = res
        .text()
        .await
        .map_err(|e| Error::Connection(format!("could not read the support service reply: {e}")))?;
    check_response(status, &body)
}

/// Send the Help form as an email to the developer.
#[tauri::command]
pub async fn support_send(state: State<'_, AppState>, request: SupportRequest) -> Result<()> {
    // The request holds what the user typed: log the topic only.
    tracing::debug!(command = "support_send", topic = %request.topic, "ipc call");
    support_send_impl(&state, &request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> SupportRequest {
        SupportRequest {
            name: " An ".into(),
            email: "an@example.com".into(),
            topic: "bug".into(),
            subject: "Crash".into(),
            message: "It crashed.".into(),
            include_system: true,
        }
    }

    fn app() -> AppInfo {
        AppInfo {
            name: "DBStudio".into(),
            version: "0.1.2".into(),
            os: "windows".into(),
            arch: "x86_64".into(),
        }
    }

    #[test]
    fn email_format() {
        for ok in ["a@b.co", "first.last+tag@sub.example.org"] {
            assert!(valid_email(ok), "{ok}");
        }
        for bad in [
            "", "a", "a@", "@b.co", "a@b", "a b@c.co", "a@b@c.co", "a@.co", "a@b.",
        ] {
            assert!(!valid_email(bad), "{bad}");
        }
    }

    #[test]
    fn validation_rejects_bad_input() {
        assert!(req().validate().is_ok());
        let mut r = req();
        r.message = "  ".into();
        assert!(matches!(r.validate(), Err(Error::Invalid(_))));
        let mut r = req();
        r.email = "nope".into();
        assert!(matches!(r.validate(), Err(Error::Invalid(_))));
        let mut r = req();
        r.topic = "spam".into();
        assert!(matches!(r.validate(), Err(Error::Invalid(_))));
        let mut r = req();
        r.subject = "x".repeat(MAX_SUBJECT + 1);
        assert!(matches!(r.validate(), Err(Error::Invalid(_))));
    }

    #[test]
    fn payload_trims_and_attaches_system_info_only_when_asked() {
        let p = req().payload(&app());
        assert_eq!(p["Name"], "An");
        assert_eq!(p["Topic"], "Bug report");
        assert_eq!(p["_subject"], "[DBStudio] Bug report: Crash");
        assert_eq!(p["_replyto"], "an@example.com");
        assert_eq!(p["App version"], "0.1.2");
        assert_eq!(p["OS"], "windows (x86_64)");
        let mut r = req();
        r.include_system = false;
        let p = r.payload(&app());
        assert!(p.get("App version").is_none());
        assert!(p.get("OS").is_none());
    }

    #[test]
    fn reply_is_read_as_success_or_error() {
        assert!(check_response(200, r#"{"success":"true","message":"ok"}"#).is_ok());
        assert!(check_response(200, r#"{"success":true}"#).is_ok());
        let e = check_response(200, r#"{"success":"false","message":"needs activation"}"#);
        assert!(e.unwrap_err().to_string().contains("needs activation"));
        let e = check_response(500, "oops");
        assert!(e.unwrap_err().to_string().contains("HTTP 500"));
    }
}
