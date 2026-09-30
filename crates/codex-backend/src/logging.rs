use crate::{config, paths};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const MAX_LOG_BYTES: u64 = 20 * 1024 * 1024;

static STDERR_SUPPRESSION_DEPTH: AtomicUsize = AtomicUsize::new(0);

pub const REDACT_KEYS: [&str; 23] = [
    "authorization",
    "proxy-authorization",
    "access",
    "access_token",
    "refresh",
    "refresh_token",
    "id_token",
    "code",
    "code_verifier",
    "chatgpt-account-id",
    "cookie",
    "set-cookie",
    "x-api-key",
    "apikey",
    "api_key",
    "token",
    "bearer_token",
    "oauth_token",
    "oauth_access_token",
    "oauth_refresh_token",
    "client_secret",
    "secret",
    "password",
];

const MAX_LOGGED_STRING_BYTES: usize = 4000;

pub fn log_file() -> std::path::PathBuf {
    paths::log_file()
}

#[must_use]
pub struct StderrSuppressionGuard;

impl Drop for StderrSuppressionGuard {
    fn drop(&mut self) {
        STDERR_SUPPRESSION_DEPTH.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn suppress_stderr() -> StderrSuppressionGuard {
    STDERR_SUPPRESSION_DEPTH.fetch_add(1, Ordering::Relaxed);
    StderrSuppressionGuard
}

fn stderr_suppressed() -> bool {
    STDERR_SUPPRESSION_DEPTH.load(Ordering::Relaxed) > 0
}

fn should_mirror_to_stderr(level: &str) -> bool {
    !stderr_suppressed() && (matches!(level, "warn" | "error") || config::log_stderr())
}

#[derive(Clone)]
pub struct Logger {
    service: String,
    base: serde_json::Map<String, Value>,
}

impl Logger {
    pub fn child(&self, bindings: serde_json::Map<String, Value>) -> Logger {
        let mut merged = self.base.clone();
        merged.extend(bindings);
        Logger {
            service: self.service.clone(),
            base: merged,
        }
    }

    pub fn debug(&self, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        self.emit("debug", msg, fields)
    }

    pub fn info(&self, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        self.emit("info", msg, fields)
    }

    pub fn warn(&self, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        self.emit("warn", msg, fields)
    }

    pub fn error(&self, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        self.emit("error", msg, fields)
    }

    fn emit(&self, level: &str, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        let mut body = serde_json::Map::new();
        body.insert("t".into(), Value::String(now_iso8601()));
        body.insert("level".into(), Value::String(level.to_string()));
        body.insert("service".into(), Value::String(self.service.clone()));
        body.insert("msg".into(), Value::String(msg.to_string()));

        let mut merged = self.base.clone();
        if let Some(fields) = fields {
            merged.extend(fields);
        }
        if !merged.is_empty() {
            body.insert("fields".into(), redact_value(Value::Object(merged)));
        }

        let line = Value::Object(body).to_string();

        let mirror_to_stderr = should_mirror_to_stderr(level);
        if mirror_to_stderr {
            let _ = writeln!(io::stderr(), "{line}");
        }

        if write_log_line(&line).is_err() && mirror_to_stderr {
            // swallow logging errors intentionally
        }
    }
}

pub fn create_logger(service: &str) -> Logger {
    Logger {
        service: service.to_string(),
        base: serde_json::Map::new(),
    }
}

fn write_log_line(line: &str) -> io::Result<()> {
    let file = log_file();
    if let Some(dir) = file.parent() {
        create_dir(dir, 0o700)?;
    }

    if fs::metadata(&file).is_ok_and(|meta| meta.len() > MAX_LOG_BYTES) {
        rotate_file(&file)?;
    }

    let mut out = OpenOptions::new().create(true).append(true).open(&file)?;
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    Ok(())
}

/// Keeps one previous log, `<name>.1`, which each rotation replaces, so the
/// log directory stays bounded at about twice `MAX_LOG_BYTES`.
fn rotate_file(path: &Path) -> io::Result<()> {
    fs::rename(path, rotated_path(path))
}

fn rotated_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

fn create_dir(path: &Path, mode: u32) -> io::Result<()> {
    fs::create_dir_all(path)?;
    set_mode(path, mode);
    Ok(())
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perm = meta.permissions();
            perm.set_mode(mode);
            let _ = fs::set_permissions(path, perm);
        }
    }
}

fn now_iso8601() -> String {
    let now = time::OffsetDateTime::now_utc();
    let format = time::format_description::parse_borrowed::<3>(
        "[year]-[month]-[day]T[hour]:[minute]:[second]Z",
    )
    .unwrap();
    now.format(&format).unwrap_or_else(|_| String::new())
}

pub fn redact_value(value: Value) -> Value {
    redact_with_depth(value, 0)
}

fn redact_with_depth(value: Value, depth: u8) -> Value {
    if depth > 6 {
        return Value::String("[depth-limit]".into());
    }

    match value {
        Value::String(s) => {
            if config::log_verbose() {
                Value::String(s)
            } else if s.len() > MAX_LOGGED_STRING_BYTES {
                // Slicing at a fixed byte offset panics inside a multi-byte
                // character, so cut at the nearest boundary before it.
                let mut cut = MAX_LOGGED_STRING_BYTES;
                while !s.is_char_boundary(cut) {
                    cut -= 1;
                }
                Value::String(format!("{}…[{} more]", &s[..cut], s.len() - cut))
            } else {
                Value::String(s)
            }
        }
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|v| redact_with_depth(v, depth + 1))
                .collect(),
        ),
        Value::Object(fields) => {
            let mut out = serde_json::Map::new();
            for (key, value) in fields {
                if REDACT_KEYS.contains(&key.to_lowercase().as_str()) {
                    out.insert(key, redact_key_redaction(value));
                } else {
                    out.insert(key, redact_with_depth(value, depth + 1));
                }
            }
            Value::Object(out)
        }
        value => value,
    }
}

fn redact_key_redaction(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(format!("[redacted len={}]", s.len())),
        _ => Value::String("[redacted]".to_string()),
    }
}

pub fn redacted_keys() -> HashSet<&'static str> {
    REDACT_KEYS.iter().copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static STDERR_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn stderr_suppression_disables_level_mirroring() {
        let _lock = STDERR_TEST_LOCK.lock().unwrap();
        assert!(should_mirror_to_stderr("warn"));

        {
            let _guard = suppress_stderr();
            assert!(!should_mirror_to_stderr("warn"));
            assert!(!should_mirror_to_stderr("error"));
        }

        assert!(should_mirror_to_stderr("warn"));
    }

    #[test]
    fn stderr_suppression_supports_nested_guards() {
        let _lock = STDERR_TEST_LOCK.lock().unwrap();
        let outer = suppress_stderr();
        let inner = suppress_stderr();
        assert!(!should_mirror_to_stderr("warn"));

        drop(inner);
        assert!(!should_mirror_to_stderr("warn"));

        drop(outer);
        assert!(should_mirror_to_stderr("warn"));
    }

    #[test]
    fn long_strings_truncate_at_a_character_boundary() {
        let _lock = STDERR_TEST_LOCK.lock().unwrap();
        // 3999 ASCII bytes put the 4000-byte cut inside a two-byte character.
        let text = format!("{}é tail", "a".repeat(3999));
        let redacted = redact_value(serde_json::json!({ "text": text }));
        let rendered = redacted["text"].as_str().unwrap();
        assert!(rendered.starts_with(&"a".repeat(3999)));
        assert!(rendered.ends_with(&format!("…[{} more]", text.len() - 3999)));
    }

    #[test]
    fn rotation_replaces_the_single_backup() {
        let directory = tempfile::TempDir::new().unwrap();
        let log = directory.path().join("proxy.log");
        for content in ["first", "second"] {
            fs::write(&log, content).unwrap();
            rotate_file(&log).unwrap();
        }
        let entries: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, ["proxy.log.1"]);
        assert_eq!(
            fs::read_to_string(directory.path().join("proxy.log.1")).unwrap(),
            "second"
        );
    }

    #[test]
    fn redacts_generic_secret_keys() {
        let redacted = redact_value(serde_json::json!({
            "token": "abc",
            "client_secret": "def",
            "Password": "ghi",
            "safe": "kept"
        }));
        assert_eq!(redacted["token"], "[redacted len=3]");
        assert_eq!(redacted["client_secret"], "[redacted len=3]");
        assert_eq!(redacted["Password"], "[redacted len=3]");
        assert_eq!(redacted["safe"], "kept");
    }

    #[test]
    fn redacts_proxy_authorization_case_insensitively() {
        let redacted = redact_value(serde_json::json!({
            "Proxy-Authorization": "Basic dXNlcjpwYXNz",
            "safe": "kept"
        }));

        assert_eq!(redacted["safe"], "kept");
        assert_eq!(redacted["Proxy-Authorization"], "[redacted len=18]");
    }
}
