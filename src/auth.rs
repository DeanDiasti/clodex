use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use codex_backend::providers::codex::auth::{
    browser_login, constants::ISSUER, device::DeviceAuthClient, jwt, pkce,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use zeroize::ZeroizeOnDrop;

#[derive(Debug)]
pub struct CredentialStatus {
    pub source: CredentialSource,
    pub path: PathBuf,
    pub account: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// Signed in with `clodex auth login`; Clodex refreshes it.
    Clodex,
    /// Reused from the Codex CLI's login; Codex refreshes it.
    CodexCli,
}

impl CredentialSource {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Clodex => "Clodex sign-in",
            Self::CodexCli => "Codex CLI login",
        }
    }
}

/// The Codex sign-in Clodex owns, stored under the Clodex home.
#[derive(Serialize, Deserialize)]
struct ClodexLogin {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

impl Drop for ClodexLogin {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.access_token.zeroize();
        self.refresh_token.zeroize();
        self.id_token.zeroize();
    }
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

    let login_path = clodex_login_path()?;
    if let Some(login) = read_clodex_login(&login_path)? {
        return Ok(CredentialStatus {
            source: CredentialSource::Clodex,
            path: login_path,
            account: login.id_token.as_deref().and_then(account_label),
        });
    }
    Ok(CredentialStatus {
        source: CredentialSource::CodexCli,
        path: codex_auth_path()?,
        account: None,
    })
}

/// Loads the Codex credentials: Clodex's own sign-in when there is one,
/// otherwise the Codex CLI's login. `refresh` renews the access token first.
pub fn load_codex_credentials(refresh: bool) -> Result<CodexCredentials> {
    let login_path = clodex_login_path()?;
    if let Some(login) = read_clodex_login(&login_path)? {
        let login = if refresh {
            refresh_clodex_login(&login_path, &login.access_token)?
        } else {
            login
        };
        return credentials_from_login(&login);
    }

    let path = codex_auth_path()?;
    if !path.exists() {
        bail!(
            "No Codex sign-in found. Run `clodex auth login` to sign in with your ChatGPT account"
        );
    }
    if refresh {
        refresh_through_codex()?;
    }
    verify_credential_file(&path)?;
    let auth_file = read_auth_file(&path)?;
    credentials_from_auth_file(auth_file)
}

/// Whether the access token expires within a few minutes, or its expiry is
/// unknown.
pub fn needs_refresh(credentials: &CodexCredentials) -> bool {
    const MARGIN_MS: u64 = 5 * 60 * 1_000;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64);
    credentials
        .expires_at_ms()
        .is_none_or(|expiry| expiry <= now_ms.saturating_add(MARGIN_MS))
}

pub struct LoginSummary {
    pub path: PathBuf,
    pub account: Option<String>,
}

/// Signs in to Codex with a ChatGPT account and stores the tokens for Clodex.
/// The device-code flow suits machines without a local browser.
pub fn login(device: bool) -> Result<LoginSummary> {
    let tokens = if device {
        DeviceAuthClient::new().run()
    } else {
        browser_login::run_browser_login_opening(&open_in_browser)
    }
    .context("Codex sign-in failed")?;
    jwt::validate_token_response(&tokens)?;

    let login = ClodexLogin {
        account_id: jwt::extract_account_id(&tokens),
        access_token: tokens.access_token.clone(),
        refresh_token: tokens.refresh_token.clone(),
        id_token: tokens.id_token.clone(),
    };
    let path = clodex_login_path()?;
    let _lock = lock_login(&path)?;
    write_clodex_login(&path, &login)?;
    Ok(LoginSummary {
        account: login.id_token.as_deref().and_then(account_label),
        path,
    })
}

/// Removes Clodex's own sign-in. Returns whether there was one.
pub fn logout() -> Result<bool> {
    let path = clodex_login_path()?;
    let _lock = lock_login(&path)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("could not remove {}", path.display())),
    }
}

pub fn clodex_login_path() -> Result<PathBuf> {
    Ok(crate::config::clodex_home()?
        .join("auth")
        .join("codex.json"))
}

fn read_clodex_login(path: &Path) -> Result<Option<ClodexLogin>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("could not inspect {}", path.display()));
        }
        Ok(_) => {}
    }
    verify_credential_file(path)?;
    let bytes = fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    let login: ClodexLogin = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "invalid Codex sign-in at {}; run `clodex auth login` again",
            path.display()
        )
    })?;
    Ok(Some(login))
}

fn write_clodex_login(path: &Path, login: &ClodexLogin) -> Result<()> {
    let directory = path.parent().context("invalid sign-in path")?;
    create_private_dir(directory)?;
    let temporary = path.with_extension("json.tmp");
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("could not create {}", temporary.display()))?;
    file.write_all(&serde_json::to_vec(login)?)?;
    file.sync_all()?;
    fs::rename(&temporary, path).with_context(|| format!("could not save {}", path.display()))
}

/// Serializes refreshes: each one spends the stored refresh token, so two at
/// once would leave one holding a token the other already rotated away.
fn lock_login(path: &Path) -> Result<fs::File> {
    let directory = path.parent().context("invalid sign-in path")?;
    create_private_dir(directory)?;
    let lock_path = path.with_extension("lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("could not open {}", lock_path.display()))?;
    FileExt::lock_exclusive(&lock)
        .with_context(|| format!("could not lock {}", lock_path.display()))?;
    Ok(lock)
}

fn create_private_dir(directory: &Path) -> Result<()> {
    fs::create_dir_all(directory)
        .with_context(|| format!("could not create {}", directory.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Renews the access token `held` was issued with. If another process already
/// renewed it, the stored result is used instead of spending the token again.
fn refresh_clodex_login(path: &Path, held: &str) -> Result<ClodexLogin> {
    let _lock = lock_login(path)?;
    let current = read_clodex_login(path)?
        .context("the Codex sign-in was removed; run `clodex auth login`")?;
    if current.access_token != held {
        return Ok(current);
    }

    let tokens = match pkce::refresh_tokens(ISSUER, &current.refresh_token) {
        Ok(tokens) => tokens,
        Err(pkce::RefreshError::Rejected(status)) => bail!(
            "the Codex sign-in has expired or was revoked (HTTP {status}); run `clodex auth login`"
        ),
        Err(error) => return Err(error.into()),
    };
    let refreshed = ClodexLogin {
        account_id: jwt::extract_account_id(&tokens).or_else(|| current.account_id.clone()),
        access_token: tokens.access_token.clone(),
        refresh_token: tokens.refresh_token.clone(),
        id_token: tokens.id_token.clone().or_else(|| current.id_token.clone()),
    };
    write_clodex_login(path, &refreshed)?;
    Ok(refreshed)
}

fn credentials_from_login(login: &ClodexLogin) -> Result<CodexCredentials> {
    if login.access_token.is_empty() {
        bail!("the Codex sign-in has an empty access token; run `clodex auth login`");
    }
    Ok(CodexCredentials {
        expires_at_ms: jwt_expiry_ms(&login.access_token),
        access_token: login.access_token.clone(),
        account_id: login.account_id.clone(),
    })
}

/// The email and plan the ID token names, for status output.
fn account_label(id_token: &str) -> Option<String> {
    let claims = jwt_claims(id_token)?;
    let email = claims.get("email")?.as_str()?;
    let plan = claims
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_plan_type")
        .and_then(serde_json::Value::as_str);
    Some(match plan {
        Some(plan) => format!("{email} ({plan})"),
        None => email.to_string(),
    })
}

fn open_in_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = Command::new(opener)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
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
    jwt_claims(token)?.get("exp")?.as_u64()?.checked_mul(1_000)
}

/// Decodes a JWT's claims without validating it; the tokens come straight
/// from the issuer, and only the issuer's servers check them.
fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn verify_credential_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "Codex file-backed credentials were not found at {}. Run `clodex auth login`, or configure Codex to use file-backed credential storage.",
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

    fn login(access: &str) -> ClodexLogin {
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            br#"{"email":"dev@example.com","https://api.openai.com/auth":{"chatgpt_plan_type":"pro"}}"#,
        );
        ClodexLogin {
            access_token: access.to_string(),
            refresh_token: "refresh".to_string(),
            id_token: Some(format!("header.{claims}.signature")),
            account_id: Some("account-1".to_string()),
        }
    }

    #[cfg(unix)]
    #[test]
    fn clodex_login_round_trips_privately() {
        use std::os::unix::fs::PermissionsExt;

        let directory = temporary_path("login-dir");
        let path = directory.join("auth").join("codex.json");
        assert!(read_clodex_login(&path).unwrap().is_none());

        write_clodex_login(&path, &login("access")).unwrap();
        let loaded = read_clodex_login(&path).unwrap().unwrap();
        assert_eq!(loaded.access_token, "access");
        assert_eq!(loaded.account_id.as_deref(), Some("account-1"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            loaded
                .id_token
                .as_deref()
                .and_then(account_label)
                .as_deref(),
            Some("dev@example.com (pro)")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_refresh_another_process_finished_is_reused() {
        let directory = temporary_path("rotated-dir");
        let path = directory.join("auth").join("codex.json");
        write_clodex_login(&path, &login("rotated")).unwrap();

        // The caller still holds the old token, so no refresh request is made.
        let refreshed = refresh_clodex_login(&path, "stale").unwrap();
        assert_eq!(refreshed.access_token, "rotated");
        fs::remove_dir_all(directory).unwrap();
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
