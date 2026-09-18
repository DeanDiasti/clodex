//! Per-launch connection notices, independent of assistant text and tool results.
use anyhow::Result;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct Watcher {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    path: PathBuf,
}

impl Watcher {
    pub fn start(command: &mut Command) -> Result<Self> {
        let id = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        let directory = crate::config::clodex_home()?.join("logs/claude-code-proxy/notifications");
        fs::create_dir_all(&directory)?;
        let path = directory.join(format!("{id}.jsonl"));
        File::create(&path)?;
        let file = File::open(&path)?;
        let headers = command
            .get_envs()
            .find(|(key, _)| *key == "ANTHROPIC_CUSTOM_HEADERS")
            .and_then(|(_, value)| value)
            .and_then(|value| value.to_str())
            .unwrap_or("");
        command
            .env(
                "ANTHROPIC_CUSTOM_HEADERS",
                format!("{headers}\nX-Clodex-Notification-Id: {id}"),
            )
            .env("CLODEX_NOTIFICATION_FILE", &path);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            let mut reader = BufReader::new(file);
            let mut line = String::new();
            loop {
                match reader.read_line(&mut line) {
                    Ok(0) if stopping.load(Ordering::Relaxed) => break,
                    Ok(0) => thread::sleep(Duration::from_millis(100)),
                    Ok(_) if line.ends_with('\n') => {
                        if let Some(notice) = notice(&line) {
                            eprintln!("\n{notice}");
                        }
                        line.clear();
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            stop,
            worker: Some(worker),
            path,
        })
    }
}
impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}
fn notice(line: &str) -> Option<String> {
    let event: Value = serde_json::from_str(line).ok()?;
    let message: String = event["message"]
        .as_str()?
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let attempt = event["attempt"].as_u64().unwrap_or(0);
    let id: String = event["reqId"]
        .as_str()
        .unwrap_or("")
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    let progress = if event["state"] == "reconnecting" {
        format!(" ({attempt}/3)")
    } else {
        String::new()
    };
    Some(format!("[Clodex {id}] {message}{progress}"))
}

pub fn statusline() -> Result<()> {
    let Some(path) = std::env::var_os("CLODEX_NOTIFICATION_FILE") else {
        return Ok(());
    };
    let Ok(mut file) = File::open(path) else {
        return Ok(());
    };
    let offset = file.metadata()?.len().saturating_sub(65_536);
    file.seek(SeekFrom::Start(offset))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let mut states = std::collections::HashMap::new();
    for line in text.lines() {
        if let Ok(event) = serde_json::from_str::<Value>(line)
            && let Some(id) = event["reqId"].as_str()
        {
            states.insert(id.to_owned(), event);
        }
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let active: Vec<_> = states
        .values()
        .filter(|event| {
            event["state"] == "reconnecting"
                && now.saturating_sub(event["time"].as_u64().unwrap_or(0)) < 90
        })
        .collect();
    match active.len() {
        0 => {}
        1 => println!("Reconnecting {}/3", active[0]["attempt"]),
        count => println!("Reconnecting {count} requests"),
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_notice_is_explicit_and_does_not_contain_control_sequences() {
        let line = r#"{"reqId":"abc12345-extra","state":"reconnecting","attempt":2,"message":"Connection interrupted; reconnecting"}"#;
        assert_eq!(
            notice(line).unwrap(),
            "[Clodex abc12345] Connection interrupted; reconnecting (2/3)"
        );
        assert!(notice("invalid").is_none());
    }
}
