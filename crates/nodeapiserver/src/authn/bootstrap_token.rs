//! Static bearer-token authentication from the kube-apiserver-compatible
//! token CSV format.
//!
//! Each non-empty line contains `token,user,uid,"group1,group2"`. The file
//! is loaded when the listener starts and refreshed when the file changes,
//! matching the apiserver's `--token-auth-file` contract. The UID is retained
//! on the authenticated result for callers that need it; [`Identity`] predates
//! token authentication and has no general UID field, so it is not folded
//! into the username or groups.

use crate::authn::x509::Identity;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

#[derive(Clone, Debug)]
pub struct Authenticator {
    tokens: HashMap<String, Entry>,
}

/// A token-file authenticator that refreshes a replaced or edited file while
/// retaining the last valid table if a reload is temporarily unreadable or
/// malformed.
#[derive(Clone, Debug)]
pub struct ReloadableAuthenticator {
    path: PathBuf,
    state: Arc<RwLock<ReloadState>>,
}

#[derive(Debug)]
struct ReloadState {
    fingerprint: FileFingerprint,
    authenticator: Authenticator,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileFingerprint {
    modified: Option<SystemTime>,
    len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    username: String,
    uid: String,
    groups: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedToken {
    pub identity: Identity,
    pub uid: String,
}

impl Authenticator {
    /// Load the standard token-auth CSV file.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let contents = fs::read_to_string(path)
            .map_err(|error| format!("reading {}: {error}", path.display()))?;
        let mut tokens = HashMap::new();
        for (line_number, line) in contents.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields = parse_csv_line(line).map_err(|error| {
                format!("{}:{}: {error}", path.display(), line_number + 1)
            })?;
            if fields.len() != 4 {
                return Err(format!(
                    "{}:{}: expected token,user,uid,groups",
                    path.display(),
                    line_number + 1
                ));
            }
            let token = fields[0].clone();
            if token.is_empty() || fields[1].is_empty() || fields[2].is_empty() {
                return Err(format!(
                    "{}:{}: token, user, and uid must not be empty",
                    path.display(),
                    line_number + 1
                ));
            }
            if tokens.contains_key(&token) {
                return Err(format!(
                    "{}:{}: duplicate token",
                    path.display(),
                    line_number + 1
                ));
            }
            let groups = fields[3]
                .split([',', ';'])
                .map(str::trim)
                .filter(|group| !group.is_empty())
                .map(str::to_string)
                .collect();
            tokens.insert(
                token,
                Entry {
                    username: fields[1].clone(),
                    uid: fields[2].clone(),
                    groups,
                },
            );
        }
        Ok(Self { tokens })
    }

    /// Authenticate one bearer token. The token itself is never exposed in
    /// the returned identity or error path.
    pub fn authenticate(&self, token: &str) -> Option<AuthenticatedToken> {
        let entry = self.tokens.get(token)?;
        Some(AuthenticatedToken {
            identity: Identity {
                name: entry.username.clone(),
                groups: entry.groups.clone(),
                uid: Some(entry.uid.clone()),
                credential_id: (String::new(), Vec::new()),
            },
            uid: entry.uid.clone(),
        })
    }
}

impl ReloadableAuthenticator {
    /// Load a token file and retain its initial fingerprint for later reloads.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let authenticator = Authenticator::from_file(path)?;
        let fingerprint = file_fingerprint(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            state: Arc::new(RwLock::new(ReloadState {
                fingerprint,
                authenticator,
            })),
        })
    }

    /// Authenticate against the latest valid file contents.
    pub fn authenticate(&self, token: &str) -> Option<AuthenticatedToken> {
        self.refresh_if_needed();
        let state = self.state.read().ok()?;
        state.authenticator.authenticate(token)
    }

    fn refresh_if_needed(&self) {
        let Ok(fingerprint) = file_fingerprint(&self.path) else {
            return;
        };
        let needs_reload = self
            .state
            .read()
            .map(|state| state.fingerprint != fingerprint)
            .unwrap_or(false);
        if !needs_reload {
            return;
        }

        let authenticator = match Authenticator::from_file(&self.path) {
            Ok(authenticator) => authenticator,
            Err(error) => {
                if let Ok(mut state) = self.state.write() {
                    state.fingerprint = fingerprint;
                }
                tracing::warn!(path = %self.path.display(), error, "token authentication file changed but could not be reloaded; retaining the last valid contents");
                return;
            }
        };
        if let Ok(mut state) = self.state.write() {
            state.fingerprint = fingerprint;
            state.authenticator = authenticator;
        }
    }
}

fn file_fingerprint(path: &Path) -> Result<FileFingerprint, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("reading {}: {error}", path.display()))?;
    Ok(FileFingerprint {
        modified: metadata.modified().ok(),
        len: metadata.len(),
    })
}

fn parse_csv_line(line: &str) -> Result<Vec<String>, String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut chars = line.chars().peekable();
    let mut quoted = false;
    let mut field_started = false;

    while let Some(character) = chars.next() {
        match character {
            '"' if !field_started => {
                quoted = true;
                field_started = true;
            }
            '"' if quoted => {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            }
            ',' if !quoted => {
                fields.push(field.trim().to_string());
                field = String::new();
                field_started = false;
            }
            _ => {
                field.push(character);
                field_started = true;
            }
        }
    }
    if quoted {
        return Err("unterminated quoted CSV field".to_string());
    }
    fields.push(field.trim().to_string());
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_quoted_groups_and_authenticates_the_token() {
        let file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file.as_file(), "abc123,bootstrap-user,uid-1,\"system:bootstrappers,devs\"")
            .unwrap();
        let auth = Authenticator::from_file(file.path()).unwrap();
        let result = auth.authenticate("abc123").unwrap();
        assert_eq!(result.identity.name, "bootstrap-user");
        assert_eq!(
            result.identity.groups,
            vec!["system:bootstrappers".to_string(), "devs".to_string()]
        );
        assert_eq!(result.uid, "uid-1");
        assert_eq!(result.identity.uid.as_deref(), Some("uid-1"));
    }

    #[test]
    fn accepts_semicolon_separated_groups_and_rejects_unknown_tokens() {
        let file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file.as_file(), "token,user,uid,group-a;group-b").unwrap();
        let auth = Authenticator::from_file(file.path()).unwrap();
        assert_eq!(auth.authenticate("missing"), None);
        assert_eq!(
            auth.authenticate("token").unwrap().identity.groups,
            vec!["group-a".to_string(), "group-b".to_string()]
        );
    }

    #[test]
    fn duplicate_tokens_are_rejected() {
        let file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file.as_file(), "token,user,uid,").unwrap();
        writeln!(file.as_file(), "token,other,uid-2,").unwrap();
        assert!(Authenticator::from_file(file.path())
            .unwrap_err()
            .contains("duplicate token"));
    }

    #[test]
    fn reloads_a_changed_token_file_without_restarting() {
        let file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file.as_file(), "old-token,old-user,uid-1,").unwrap();
        let auth = ReloadableAuthenticator::from_file(file.path()).unwrap();
        assert_eq!(auth.authenticate("old-token").unwrap().identity.name, "old-user");

        std::fs::write(file.path(), "new-token,new-user,uid-2,group\n").unwrap();
        assert_eq!(auth.authenticate("old-token"), None);
        assert_eq!(auth.authenticate("new-token").unwrap().identity.name, "new-user");
    }

    #[test]
    fn keeps_the_last_valid_table_when_a_reload_is_malformed() {
        let file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file.as_file(), "token,user,uid,").unwrap();
        let auth = ReloadableAuthenticator::from_file(file.path()).unwrap();

        std::fs::write(file.path(), "malformed\n").unwrap();
        assert_eq!(auth.authenticate("token").unwrap().identity.name, "user");
    }
}
