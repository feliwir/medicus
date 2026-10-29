//! Local accounts (ADR-0004 §5): users with argon2id password hashes, browser sessions and API
//! tokens, and the setup token of a server without users.
//!
//! Sessions and tokens are random secrets handed to the client once; the index stores only their
//! SHA-256, so reading the index does not yield anything that can be presented as a credential.
//! Users are principals (ADR-0003): their roles decide what they may do.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString};
use medicus_core::capability::{Credential, CredentialKind, IndexStore, User};
use medicus_core::{Error, ErrorKind, Result};
use sha2::{Digest as _, Sha256};
use time::{Duration, OffsetDateTime};

pub(crate) use medicus_core::access::SESSION_COOKIE;
/// How long a session lasts after login.
pub(crate) const SESSION_TTL: Duration = Duration::hours(12);
const MIN_PASSWORD_CHARS: usize = 8;
/// `last_used_at` is written at most this often per credential.
const TOUCH_INTERVAL: Duration = Duration::minutes(5);

/// A new random secret with a prefix that tells what it is (`mds_` sessions, `mdt_` tokens,
/// `mdsetup_` the setup token).
fn new_secret(prefix: &str) -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| Error::internal(format!("no random numbers available: {e}")))?;
    Ok(format!("{prefix}{}", hex::encode(bytes)))
}

/// What the index stores of a secret.
pub(crate) fn secret_hash(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

fn hash_password(password: &str) -> Result<String> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt)
        .map_err(|e| Error::internal(format!("no random numbers available: {e}")))?;
    let salt = SaltString::encode_b64(&salt).map_err(|e| Error::internal(e.to_string()))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| Error::internal(format!("hashing the password: {e}")))
}

fn verify_password(hash: &str, password: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

/// A hash to verify against when the user does not exist, so that a login takes as long for an
/// unknown user as for a wrong password.
fn dummy_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| hash_password("not a password of anyone").unwrap_or_default())
}

/// Runs the deliberately slow password hashing off the async workers.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::internal(format!("password hashing failed: {e}")))
}

fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'));
    if valid {
        Ok(())
    } else {
        Err(Error::invalid_argument(format!(
            "invalid user name {name:?}: use up to 64 letters, digits and . _ - @"
        )))
    }
}

fn validate_password(password: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(Error::invalid_argument(format!(
            "the password must have at least {MIN_PASSWORD_CHARS} characters"
        )));
    }
    Ok(())
}

/// A user and a new session or token secret for them.
#[derive(Debug)]
pub(crate) struct Issued {
    pub(crate) user: User,
    #[allow(dead_code)] // listed by the token endpoints (phase 7)
    pub(crate) credential: Credential,
    /// Handed to the client once; only its hash is stored.
    pub(crate) secret: String,
}

pub(crate) struct Accounts {
    index: Arc<dyn IndexStore>,
    /// Whether a user exists. Once set it stays set: removing users does not reopen setup.
    has_users: AtomicBool,
    /// The one-time setup token while no user exists.
    setup_token: Mutex<Option<String>>,
}

impl Accounts {
    /// The state store the accounts live in; it also holds the settings.
    pub(crate) fn state(&self) -> &Arc<dyn IndexStore> {
        &self.index
    }
}

impl std::fmt::Debug for Accounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Accounts")
            .field("in_setup", &self.in_setup())
            .finish_non_exhaustive()
    }
}

impl Accounts {
    /// Accounts in `index`. Without any user the server is in setup mode, and a setup token is
    /// generated.
    pub(crate) async fn open(index: Arc<dyn IndexStore>) -> Result<Self> {
        let has_users = !index.list_users().await?.is_empty();
        let setup_token = if has_users {
            None
        } else {
            Some(new_secret("mdsetup_")?)
        };
        Ok(Self {
            index,
            has_users: AtomicBool::new(has_users),
            setup_token: Mutex::new(setup_token),
        })
    }

    /// Whether the server waits for its first user.
    pub(crate) fn in_setup(&self) -> bool {
        !self.has_users.load(Ordering::Acquire)
    }

    /// The setup token, while in setup mode.
    pub(crate) fn setup_token(&self) -> Option<String> {
        self.setup_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Creates the first user, an administrator, if `token` is the setup token; logs them in.
    pub(crate) async fn setup(
        &self,
        token: &str,
        name: &str,
        password: &str,
        display_name: Option<String>,
    ) -> Result<Issued> {
        let expected = self
            .setup_token()
            .ok_or_else(|| Error::new(ErrorKind::Rejected, "the server is set up already"))?;
        // Comparing hashes keeps the comparison's timing independent of the token.
        if secret_hash(token) != secret_hash(&expected) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "the setup token is not valid",
            ));
        }
        let user = self
            .create_user(
                name,
                Some(password),
                vec![crate::access::ADMIN_ROLE.to_owned()],
                display_name,
            )
            .await?;
        *self
            .setup_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        tracing::info!(user = %user.name, "setup finished: administrator created");
        self.issue(user, CredentialKind::Session, None, Some(SESSION_TTL))
            .await
    }

    pub(crate) async fn create_user(
        &self,
        name: &str,
        password: Option<&str>,
        roles: Vec<String>,
        display_name: Option<String>,
    ) -> Result<User> {
        validate_name(name)?;
        let password_hash = match password {
            Some(password) => {
                validate_password(password)?;
                let password = password.to_owned();
                Some(blocking(move || hash_password(&password)).await??)
            }
            None => None,
        };
        let now = OffsetDateTime::now_utc();
        let user = User {
            name: name.to_owned(),
            display_name,
            password_hash,
            roles,
            disabled: false,
            created_at: now,
            updated_at: now,
        };
        self.index.create_user(&user).await?;
        self.has_users.store(true, Ordering::Release);
        Ok(user)
    }

    pub(crate) async fn set_password(&self, name: &str, password: &str) -> Result<()> {
        validate_password(password)?;
        let mut user = self
            .index
            .get_user(name)
            .await?
            .ok_or_else(|| Error::not_found(format!("no user {name:?}")))?;
        let password = password.to_owned();
        user.password_hash = Some(blocking(move || hash_password(&password)).await??);
        user.updated_at = OffsetDateTime::now_utc();
        self.index.update_user(&user).await
    }

    /// Checks a user's password and starts a session; `None` for an unknown or disabled user or
    /// a wrong password (deliberately indistinguishable).
    pub(crate) async fn login(&self, name: &str, password: &str) -> Result<Option<Issued>> {
        let user = self.index.get_user(name).await?;
        let hash = user
            .as_ref()
            .and_then(|u| u.password_hash.clone())
            .unwrap_or_else(|| dummy_hash().to_owned());
        let password = password.to_owned();
        let valid = blocking(move || verify_password(&hash, &password)).await?;
        let Some(user) = user.filter(|u| valid && !u.disabled && u.password_hash.is_some()) else {
            return Ok(None);
        };
        tracing::info!(user = %user.name, "logged in");
        self.issue(user, CredentialKind::Session, None, Some(SESSION_TTL))
            .await
            .map(Some)
    }

    /// Creates an API token for `user`; `ttl` `None` never expires.
    pub(crate) async fn create_token(
        &self,
        user: &str,
        name: Option<String>,
        ttl: Option<Duration>,
    ) -> Result<Issued> {
        let user = self
            .index
            .get_user(user)
            .await?
            .ok_or_else(|| Error::not_found(format!("no user {user:?}")))?;
        self.issue(user, CredentialKind::ApiToken, name, ttl).await
    }

    async fn issue(
        &self,
        user: User,
        kind: CredentialKind,
        name: Option<String>,
        ttl: Option<Duration>,
    ) -> Result<Issued> {
        let secret = new_secret(match kind {
            CredentialKind::Session => "mds_",
            CredentialKind::ApiToken => "mdt_",
        })?;
        let now = OffsetDateTime::now_utc();
        let credential = Credential {
            id: uuid::Uuid::now_v7(),
            user: user.name.clone(),
            kind,
            secret_hash: secret_hash(&secret),
            name,
            created_at: now,
            expires_at: ttl.map(|ttl| now + ttl),
            last_used_at: None,
        };
        self.index.put_credential(&credential).await?;
        Ok(Issued {
            user,
            credential,
            secret,
        })
    }

    /// The user a session or token secret belongs to; `None` if the secret is unknown or
    /// expired, or the user is disabled.
    pub(crate) async fn authenticate(&self, secret: &str) -> Result<Option<User>> {
        let now = OffsetDateTime::now_utc();
        let Some(mut credential) = self.index.find_credential(&secret_hash(secret)).await? else {
            return Ok(None);
        };
        if credential.is_expired(now) {
            return Ok(None);
        }
        let Some(user) = self
            .index
            .get_user(&credential.user)
            .await?
            .filter(|u| !u.disabled)
        else {
            return Ok(None);
        };
        if credential
            .last_used_at
            .is_none_or(|t| now - t >= TOUCH_INTERVAL)
        {
            credential.last_used_at = Some(now);
            if let Err(e) = self.index.put_credential(&credential).await {
                tracing::debug!(error = %e, "cannot record the credential's use");
            }
        }
        Ok(Some(user))
    }

    /// Ends the session (or revokes the token) with this secret; unknown secrets are ignored.
    pub(crate) async fn logout(&self, secret: &str) -> Result<()> {
        if let Some(credential) = self.index.find_credential(&secret_hash(secret)).await? {
            match self.index.delete_credential(credential.id).await {
                Err(e) if e.kind != ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }

    /// Whether `password` is the user's (for changing it); `false` for unknown users.
    pub(crate) async fn verify(&self, name: &str, password: &str) -> Result<bool> {
        let hash = self
            .index
            .get_user(name)
            .await?
            .and_then(|u| u.password_hash)
            .unwrap_or_else(|| dummy_hash().to_owned());
        let password = password.to_owned();
        blocking(move || verify_password(&hash, &password)).await
    }

    /// Replaces a user (validating a new password); disabling them ends their sessions.
    pub(crate) async fn update_user(&self, mut user: User, password: Option<&str>) -> Result<User> {
        if let Some(password) = password {
            validate_password(password)?;
            let password = password.to_owned();
            user.password_hash = Some(blocking(move || hash_password(&password)).await??);
        }
        user.updated_at = OffsetDateTime::now_utc();
        self.index.update_user(&user).await?;
        if user.disabled {
            self.end_sessions(&user.name).await?;
        }
        Ok(user)
    }

    /// Ends a user's browser sessions; their API tokens stay.
    pub(crate) async fn end_sessions(&self, name: &str) -> Result<()> {
        for c in self.index.list_credentials(name).await? {
            if c.kind == CredentialKind::Session {
                match self.index.delete_credential(c.id).await {
                    Err(e) if e.kind != ErrorKind::NotFound => return Err(e),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// A user's API tokens.
    pub(crate) async fn tokens(&self, name: &str) -> Result<Vec<Credential>> {
        Ok(self
            .index
            .list_credentials(name)
            .await?
            .into_iter()
            .filter(|c| c.kind == CredentialKind::ApiToken)
            .collect())
    }

    /// An enabled user, for resolving a principal claim.
    pub(crate) async fn user(&self, name: &str) -> Result<Option<User>> {
        Ok(self.index.get_user(name).await?.filter(|u| !u.disabled))
    }

    /// Deletes expired sessions and tokens.
    pub(crate) async fn prune(&self) -> Result<u64> {
        self.index
            .prune_credentials(OffsetDateTime::now_utc())
            .await
    }
}

/// User accounts in a server's state store, for the `medicus users` commands. The server itself does
/// not need to run.
#[derive(Debug)]
pub struct UserStore {
    accounts: Accounts,
    /// Keeps the index addon loaded; knows the roles.
    store: crate::SettingsStore,
}

/// A user as the `users` commands show it.
#[derive(Debug, serde::Serialize)]
pub struct UserSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub roles: Vec<String>,
    pub disabled: bool,
    pub password: bool,
}

impl From<User> for UserSummary {
    fn from(u: User) -> Self {
        Self {
            name: u.name,
            display_name: u.display_name,
            roles: u.roles,
            disabled: u.disabled,
            password: u.password_hash.is_some(),
        }
    }
}

impl UserStore {
    /// Opens the state store of the server configured by `bootstrap`.
    pub async fn open(bootstrap: crate::Config) -> anyhow::Result<Self> {
        let store = crate::SettingsStore::open(bootstrap)?;
        Ok(Self {
            accounts: Accounts::open(store.index()).await?,
            store,
        })
    }

    /// Creates a user; `admin` adds the built-in `admin` role.
    pub async fn create(
        &self,
        name: &str,
        password: Option<&str>,
        admin: bool,
        mut roles: Vec<String>,
        display_name: Option<String>,
    ) -> anyhow::Result<UserSummary> {
        if admin && !roles.iter().any(|r| r == crate::access::ADMIN_ROLE) {
            roles.insert(0, crate::access::ADMIN_ROLE.to_owned());
        }
        let settings = self.store.current().await?;
        if let Some(unknown) = roles
            .iter()
            .find(|r| *r != crate::access::ADMIN_ROLE && !settings.roles.contains_key(*r))
        {
            anyhow::bail!(
                "there is no role {unknown:?}; the settings define {:?}, and `admin` is built in",
                settings.roles.keys().collect::<Vec<_>>()
            );
        }
        Ok(self
            .accounts
            .create_user(name, password, roles, display_name)
            .await?
            .into())
    }

    pub async fn set_password(&self, name: &str, password: &str) -> anyhow::Result<()> {
        Ok(self.accounts.set_password(name, password).await?)
    }

    pub async fn list(&self) -> anyhow::Result<Vec<UserSummary>> {
        Ok(self
            .accounts
            .index
            .list_users()
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    /// Enables or disables a user; disabling also ends their sessions (their tokens stay, but are
    /// not accepted while the user is disabled).
    pub async fn set_disabled(&self, name: &str, disabled: bool) -> anyhow::Result<()> {
        let index = &self.accounts.index;
        let mut user = index
            .get_user(name)
            .await?
            .ok_or_else(|| Error::not_found(format!("no user {name:?}")))?;
        user.disabled = disabled;
        self.accounts.update_user(user, None).await?;
        Ok(())
    }

    /// Deletes a user and their sessions and tokens.
    pub async fn delete(&self, name: &str) -> anyhow::Result<()> {
        Ok(self.accounts.index.delete_user(name).await?)
    }

    /// Creates an API token; returns its secret, which is shown only now.
    pub async fn create_token(
        &self,
        user: &str,
        label: Option<String>,
        days: Option<u32>,
    ) -> anyhow::Result<String> {
        let ttl = days.map(|d| Duration::days(i64::from(d)));
        Ok(self.accounts.create_token(user, label, ttl).await?.secret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn accounts(dir: &std::path::Path) -> (Accounts, crate::SettingsStore) {
        let mut cfg = crate::Config::default();
        cfg.server.data_dir = dir.to_owned();
        let store = crate::SettingsStore::open(cfg).unwrap();
        (Accounts::open(store.index()).await.unwrap(), store)
    }

    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::too_many_lines)] // one scenario, step by step
    async fn setup_login_tokens_and_logout() {
        let dir = tempfile::tempdir().unwrap();
        let (accounts, store) = accounts(dir.path()).await;
        assert!(accounts.in_setup());
        let token = accounts.setup_token().unwrap();
        assert!(token.starts_with("mdsetup_"));

        // The setup token is checked, and the password policy applies.
        let wrong = accounts.setup("nope", "admin", "long enough", None).await;
        assert_eq!(wrong.unwrap_err().kind, ErrorKind::PermissionDenied);
        let short = accounts.setup(&token, "admin", "short", None).await;
        assert_eq!(short.unwrap_err().kind, ErrorKind::InvalidArgument);

        let issued = accounts
            .setup(&token, "admin", "long enough", None)
            .await
            .unwrap();
        assert!(!accounts.in_setup());
        assert_eq!(issued.user.roles, [crate::access::ADMIN_ROLE]);
        assert!(issued.secret.starts_with("mds_"));
        assert_ne!(issued.credential.secret_hash, issued.secret);
        assert_eq!(
            accounts
                .setup(&token, "other", "long enough", None)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Rejected,
            "the setup token works once"
        );
        let user = accounts
            .authenticate(&issued.secret)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.name, "admin");

        // Logins: wrong passwords and unknown users look the same.
        assert!(
            accounts
                .login("admin", "wrong password")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            accounts
                .login("nobody", "long enough")
                .await
                .unwrap()
                .is_none()
        );
        let session = accounts
            .login("admin", "long enough")
            .await
            .unwrap()
            .unwrap();
        accounts.logout(&session.secret).await.unwrap();
        assert!(
            accounts
                .authenticate(&session.secret)
                .await
                .unwrap()
                .is_none()
        );

        // Tokens, and users without a password (tokens only).
        accounts
            .create_user("ci", None, vec![], None)
            .await
            .unwrap();
        assert!(accounts.login("ci", "").await.unwrap().is_none());
        let token = accounts
            .create_token("ci", Some("pipeline".into()), None)
            .await
            .unwrap();
        assert!(token.secret.starts_with("mdt_"));
        assert_eq!(
            accounts
                .authenticate(&token.secret)
                .await
                .unwrap()
                .unwrap()
                .name,
            "ci"
        );
        let expired = accounts
            .create_token("ci", None, Some(Duration::seconds(-1)))
            .await
            .unwrap();
        assert!(
            accounts
                .authenticate(&expired.secret)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(accounts.prune().await.unwrap(), 1);

        // Passwords can be changed; disabled users can neither log in nor use their tokens.
        accounts
            .set_password("admin", "a new password")
            .await
            .unwrap();
        assert!(
            accounts
                .login("admin", "long enough")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            accounts
                .login("admin", "a new password")
                .await
                .unwrap()
                .is_some()
        );
        let mut ci = store.index().get_user("ci").await.unwrap().unwrap();
        ci.disabled = true;
        store.index().update_user(&ci).await.unwrap();
        assert!(
            accounts
                .authenticate(&token.secret)
                .await
                .unwrap()
                .is_none()
        );
        assert!(accounts.user("ci").await.unwrap().is_none());
        assert_eq!(
            accounts
                .create_user("bad name", None, vec![], None)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::InvalidArgument
        );
    }
}
