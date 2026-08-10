use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::Deserialize;
use zeroize::ZeroizeOnDrop;

#[derive(Debug)]
pub struct CredentialStatus {
    pub auth_mode: String,
    pub source: PathBuf,
}

#[derive(ZeroizeOnDrop)]
pub struct CodexCredentials {
    access_token: String,
    account_id: Option<String>,
    expires_at_ms: Option<u64>,
}

impl CodexCredentials {
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    pub fn expires_at_ms(&self) -> Option<u64> {
        self.expires_at_ms
    }

    pub fn has_same_access_token(&self, other: &Self) -> bool {
        self.access_token == other.access_token
    }

    #[cfg(test)]
    pub fn for_test(
        access_token: &str,
        account_id: Option<&str>,
        expires_at_ms: Option<u64>,
    ) -> Self {
        Self {
            access_token: access_token.to_string(),
            account_id: account_id.map(str::to_string),
            expires_at_ms,
        }
    }
}

#[derive(Deserialize)]
struct AuthFile {
    auth_mode: String,
    tokens: TokenSet,
}

#[derive(Deserialize)]
struct TokenSet {
    access_token: String,
    #[serde(default)]
    account_id: Option<String>,
}

pub fn prepare_codex_credentials() -> Result<CredentialStatus> {
    let credentials = load_codex_credentials(false)?;
    if credentials.access_token().is_empty() {
        bail!("Codex access token is empty");
    }

    let auth_file = read_auth_file(&codex_auth_path()?)?;
    Ok(CredentialStatus {
        auth_mode: auth_file.auth_mode,
        source: codex_auth_path()?,
    })
}

pub fn load_codex_credentials(refresh: bool) -> Result<CodexCredentials> {
    if refresh {
        refresh_through_codex()?;
    }

    let path = codex_auth_path()?;
    verify_credential_file(&path)?;
    let auth_file = read_auth_file(&path)?;
    credentials_from_auth_file(auth_file)
}

fn credentials_from_auth_file(auth_file: AuthFile) -> Result<CodexCredentials> {
    if auth_file.auth_mode != "chatgpt" {
        bail!(
            "Codex is authenticated with {:?}, not a reusable ChatGPT subscription login",
            auth_file.auth_mode
        );
    }
    if auth_file.tokens.access_token.is_empty() {
        bail!("Codex credential file contains an empty access token");
    }

    Ok(CodexCredentials {
        expires_at_ms: jwt_expiry_ms(&auth_file.tokens.access_token),
        access_token: auth_file.tokens.access_token,
        account_id: auth_file.tokens.account_id,
    })
}

pub fn codex_auth_path() -> Result<PathBuf> {
    if let Some(codex_home) = env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(codex_home).join("auth.json"));
    }

    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".codex").join("auth.json"))
}

fn refresh_through_codex() -> Result<()> {
    let mut child = Command::new("codex")
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("could not start Codex App Server to refresh the managed login")?;

    let mut stdin = child
        .stdin
        .take()
        .context("Codex App Server did not expose stdin")?;
    let stdout = child
        .stdout
        .take()
        .context("Codex App Server did not expose stdout")?;

    for message in [
        serde_json::json!({
            "method": "initialize",
            "id": 0,
            "params": {
                "clientInfo": {
                    "name": "clodex",
                    "title": "Clodex",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        }),
        serde_json::json!({"method": "initialized", "params": {}}),
        serde_json::json!({
            "method": "account/read",
            "id": 1,
            "params": {"refreshToken": true}
        }),
    ] {
        writeln!(stdin, "{}", serde_json::to_string(&message)?)?;
    }
    stdin.flush()?;

    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut found = None;
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(serde_json::Value::as_u64) == Some(1) {
                found = Some(value);
                break;
            }
        }
        let _ = sender.send(found);
    });

    let response = receiver.recv_timeout(Duration::from_secs(15));
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();

    let response = response
        .context("timed out waiting for Codex to refresh its managed login")?
        .context("Codex App Server closed before refreshing the login")?;
    if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
        bail!("Codex could not refresh its managed login: {error}");
    }
    let account_type = response
        .pointer("/result/account/type")
        .and_then(serde_json::Value::as_str);
    if account_type != Some("chatgpt") {
        bail!("Codex App Server did not report a managed ChatGPT login after refresh");
    }
    Ok(())
}

fn read_auth_file(path: &Path) -> Result<AuthFile> {
    let bytes = fs::read(path)
        .with_context(|| format!("could not read Codex credentials at {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid Codex credential file at {}", path.display()))
}

fn jwt_expiry_ms(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims.get("exp")?.as_u64()?.checked_mul(1_000)
}

fn verify_credential_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "Codex file-backed credentials were not found at {}. Run `codex login`, or configure Codex to use file-backed credential storage.",
            path.display()
        )
    })?;

    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to read Codex credentials through symlink {}",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!("Codex credential path is not a file: {}", path.display());
    }

    verify_unix_permissions(path, &metadata)
}

#[cfg(unix)]
fn verify_unix_permissions(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let mode = metadata.mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "Codex credential file {} has unsafe permissions {:o}; expected 600 or stricter",
            path.display(),
            mode
        );
    }

    let current_uid = unsafe { libc_geteuid() };
    if metadata.uid() != current_uid {
        bail!(
            "Codex credential file {} is owned by uid {}, not current uid {}",
            path.display(),
            metadata.uid(),
            current_uid
        );
    }

    Ok(())
}

#[cfg(unix)]
unsafe fn libc_geteuid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() }
}

#[cfg(not(unix))]
fn verify_unix_permissions(_path: &Path, _metadata: &fs::Metadata) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temporary_path(test_name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("clodex-auth-{test_name}-{nonce}.json"))
    }

    fn write_auth(path: &Path, mode: &str, token: &str) {
        fs::write(
            path,
            format!(
                r#"{{"auth_mode":"{mode}","tokens":{{"access_token":"{token}","account_id":"account-1","refresh_token":"not-read"}}}}"#
            ),
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn reads_only_required_auth_fields() {
        let path = temporary_path("valid");
        write_auth(&path, "chatgpt", "secret-token");

        verify_credential_file(&path).unwrap();
        let auth = read_auth_file(&path).unwrap();

        assert_eq!(auth.auth_mode, "chatgpt");
        assert_eq!(auth.tokens.access_token, "secret-token");
        assert_eq!(auth.tokens.account_id.as_deref(), Some("account-1"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn extracts_expiry_from_a_jwt_without_validating_it() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":12345}"#);
        assert_eq!(
            jwt_expiry_ms(&format!("header.{payload}.signature")),
            Some(12_345_000)
        );
        assert_eq!(jwt_expiry_ms("not-a-jwt"), None);
        assert_eq!(jwt_expiry_ms("header.not-base64.signature"), None);

        let missing_exp =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"sub":"user"}"#);
        assert_eq!(
            jwt_expiry_ms(&format!("header.{missing_exp}.signature")),
            None
        );

        let overflowing = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!(r#"{{"exp":{}}}"#, u64::MAX));
        assert_eq!(
            jwt_expiry_ms(&format!("header.{overflowing}.signature")),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_broad_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = temporary_path("permissions");
        write_auth(&path, "chatgpt", "secret-token");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        assert!(verify_credential_file(&path).is_err());
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_credentials() {
        use std::os::unix::fs::symlink;

        let target = temporary_path("target");
        let link = temporary_path("link");
        write_auth(&target, "chatgpt", "secret-token");
        symlink(&target, &link).unwrap();

        assert!(verify_credential_file(&link).is_err());
        fs::remove_file(link).unwrap();
        fs::remove_file(target).unwrap();
    }

    #[test]
    fn rejects_missing_invalid_and_non_file_credentials() {
        let missing = temporary_path("missing");
        assert!(verify_credential_file(&missing).is_err());

        let malformed = temporary_path("malformed");
        fs::write(&malformed, b"not json").unwrap();
        assert!(read_auth_file(&malformed).is_err());
        fs::remove_file(malformed).unwrap();

        let directory = temporary_path("directory");
        fs::create_dir(&directory).unwrap();
        assert!(verify_credential_file(&directory).is_err());
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn accepts_only_nonempty_chatgpt_access_tokens() {
        let wrong_mode = AuthFile {
            auth_mode: "apikey".to_string(),
            tokens: TokenSet {
                access_token: "secret".to_string(),
                account_id: None,
            },
        };
        assert!(credentials_from_auth_file(wrong_mode).is_err());

        let empty = AuthFile {
            auth_mode: "chatgpt".to_string(),
            tokens: TokenSet {
                access_token: String::new(),
                account_id: None,
            },
        };
        assert!(credentials_from_auth_file(empty).is_err());

        let valid = AuthFile {
            auth_mode: "chatgpt".to_string(),
            tokens: TokenSet {
                access_token: "opaque-token".to_string(),
                account_id: None,
            },
        };
        let credentials = credentials_from_auth_file(valid).unwrap();
        assert_eq!(credentials.access_token(), "opaque-token");
        assert_eq!(credentials.account_id(), None);
        assert_eq!(credentials.expires_at_ms(), None);
    }
}
