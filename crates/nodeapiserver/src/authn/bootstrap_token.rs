//! Static bearer-token authentication from the kube-apiserver-compatible
//! token CSV format.
//!
//! Each non-empty line contains `token,user,uid,"group1,group2"`. The file
//! is loaded once when the listener starts, matching the apiserver's
//! `--token-auth-file` startup configuration. The UID is retained on the
//! authenticated result for callers that need it; [`Identity`] predates
//! token authentication and has no general UID field, so it is not folded
//! into the username or groups.

use crate::authn::x509::Identity;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct Authenticator {
    tokens: HashMap<String, Entry>,
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
                credential_id: (String::new(), Vec::new()),
            },
            uid: entry.uid.clone(),
        })
    }
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
}
