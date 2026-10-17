//! The GitHub sign-in used for GitHub Copilot cleanup.
//!
//! OpenWritr looks in the same places, in the same order, as GitHub Copilot
//! CLI itself, so the account the `copilot` command is signed in with is the
//! one OpenWritr uses:
//!
//! 1. the `COPILOT_GITHUB_TOKEN`, `GH_TOKEN` and `GITHUB_TOKEN` variables;
//! 2. the token `copilot login` saved in Windows Credential Manager (an entry
//!    named after `copilot-cli`);
//! 3. the plaintext fallback Copilot CLI offers when Credential Manager is not
//!    available (`config.json` in `%USERPROFILE%\.copilot`, or `COPILOT_HOME`);
//! 4. the GitHub CLI (`gh auth token`), which Copilot CLI also falls back to.
//!
//! Tokens are never logged; only where one was found.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use thiserror::Error;
use tracing::{info, warn};

/// Where the token came from (shown in Settings; never the token itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenSource {
    Environment(&'static str),
    CopilotCli,
    CopilotCliConfigFile,
    GithubCli,
}

impl TokenSource {
    pub fn describe(self) -> String {
        match self {
            TokenSource::Environment(name) => format!("the {name} environment variable"),
            TokenSource::CopilotCli => "GitHub Copilot CLI".into(),
            TokenSource::CopilotCliConfigFile => "GitHub Copilot CLI (its config file)".into(),
            TokenSource::GithubCli => "GitHub CLI (gh)".into(),
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CopilotAuthError {
    #[error(
        "GitHub Copilot CLI was not found. Install it (winget install GitHub.Copilot), \
         run `copilot login`, then try again"
    )]
    CliMissing,
    #[error("GitHub Copilot CLI is not signed in. Run `copilot login`, then try again")]
    NotAuthenticated,
    #[error("GitHub sign-in lookup failed: {0}")]
    CommandFailed(String),
}

/// Variables Copilot CLI reads a token from, in its order.
const TOKEN_VARIABLES: [&str; 3] = ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"];

/// How long a found token is reused before looking again.
const CACHE_FOR: Duration = Duration::from_secs(600);

/// The token to send to GitHub Copilot, and where it came from.
pub fn copilot_token() -> Result<(String, TokenSource), CopilotAuthError> {
    static CACHE: Mutex<Option<(String, TokenSource, Instant)>> = Mutex::new(None);
    if let Some((token, source, at)) = CACHE.lock().as_ref() {
        if at.elapsed() < CACHE_FOR {
            return Ok((token.clone(), *source));
        }
    }
    let (token, source) = find_token()?;
    info!(source = %source.describe(), "found a GitHub Copilot sign-in");
    *CACHE.lock() = Some((token.clone(), source, Instant::now()));
    Ok((token, source))
}

fn find_token() -> Result<(String, TokenSource), CopilotAuthError> {
    for name in TOKEN_VARIABLES {
        if let Some(token) = std::env::var(name)
            .ok()
            .and_then(|value| usable_variable(&value))
        {
            return Ok((token, TokenSource::Environment(name)));
        }
    }
    if let Some(token) = credential_manager_token() {
        return Ok((token, TokenSource::CopilotCli));
    }
    if let Some(token) = std::fs::read_to_string(copilot_home().join("config.json"))
        .ok()
        .and_then(|text| find_github_token(&text))
    {
        return Ok((token, TokenSource::CopilotCliConfigFile));
    }
    match gh_token() {
        Ok(token) => Ok((token, TokenSource::GithubCli)),
        // Copilot CLI is there but nothing was found: it is not signed in.
        Err(_) if locate_copilot().is_some() => Err(CopilotAuthError::NotAuthenticated),
        Err(error) => Err(error),
    }
}

/// A token from an environment variable, if Copilot accepts its kind.
/// Classic personal access tokens (`ghp_`) are not accepted by Copilot.
fn usable_variable(value: &str) -> Option<String> {
    let token = value.trim();
    (!token.is_empty() && !token.starts_with("ghp_")).then(|| token.to_string())
}

/// Copilot CLI's configuration folder.
fn copilot_home() -> PathBuf {
    if let Some(home) = std::env::var_os("COPILOT_HOME").filter(|home| !home.is_empty()) {
        return PathBuf::from(home);
    }
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".copilot")
}

/// Kinds of GitHub token Copilot accepts: OAuth (`gho_`), GitHub App user
/// (`ghu_`) and fine-grained personal access tokens (`github_pat_`).
const TOKEN_PREFIXES: [&str; 3] = ["github_pat_", "gho_", "ghu_"];
/// Shortest token taken for real (GitHub's are 40 characters or more).
const MIN_TOKEN_CHARS: usize = 30;

/// The first GitHub token in `text`, which may be the bare token or a JSON
/// document holding it.
fn find_github_token(text: &str) -> Option<String> {
    TOKEN_PREFIXES
        .iter()
        .flat_map(|prefix| text.match_indices(prefix).map(|(at, _)| at))
        .filter_map(|at| {
            let token: String = text[at..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            (token.len() >= MIN_TOKEN_CHARS).then_some((at, token))
        })
        .min_by_key(|(at, _)| *at)
        .map(|(_, token)| token)
}

/// A token in a Credential Manager entry: tools write the secret as UTF-8 or
/// as UTF-16.
fn token_from_blob(blob: &[u8]) -> Option<String> {
    find_github_token(&String::from_utf8_lossy(blob)).or_else(|| {
        let wide: Vec<u16> = blob
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        find_github_token(&String::from_utf16_lossy(&wide))
    })
}

/// Whether a Credential Manager entry belongs to Copilot CLI. Its exact name
/// is not documented (`copilot-cli`, or with the account added), so any entry
/// whose name contains it counts.
fn is_copilot_cli_entry(target: &str) -> bool {
    target.to_ascii_lowercase().contains("copilot-cli")
}

/// The token `copilot login` saved in Windows Credential Manager. With several
/// accounts, the most recently saved one.
fn credential_manager_token() -> Option<String> {
    use windows_sys::Win32::Security::Credentials::{CredEnumerateW, CredFree, CREDENTIALW};

    let mut count = 0_u32;
    let mut list: *mut *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: both out-pointers are valid; on success Windows hands back an
    // array of `count` credentials that is released with `CredFree` below.
    let ok = unsafe { CredEnumerateW(std::ptr::null(), 0, &mut count, &mut list) };
    if ok == 0 || list.is_null() {
        return None;
    }

    let mut entries = 0_usize;
    let mut newest: Option<(u64, String)> = None;
    for index in 0..count as usize {
        // SAFETY: `index < count`, and every element points to a credential.
        let credential = unsafe { &**list.add(index) };
        // SAFETY: Windows gives a valid NUL-terminated name or null.
        let target = unsafe { wide_to_string(credential.TargetName) };
        if !is_copilot_cli_entry(&target) {
            continue;
        }
        entries += 1;
        let blob: &[u8] = if credential.CredentialBlob.is_null() {
            &[]
        } else {
            // SAFETY: the blob is `CredentialBlobSize` bytes long.
            unsafe {
                std::slice::from_raw_parts(
                    credential.CredentialBlob,
                    credential.CredentialBlobSize as usize,
                )
            }
        };
        let Some(token) = token_from_blob(blob) else {
            continue;
        };
        let written = (u64::from(credential.LastWritten.dwHighDateTime) << 32)
            | u64::from(credential.LastWritten.dwLowDateTime);
        if newest.as_ref().map_or(true, |(at, _)| written > *at) {
            newest = Some((written, token));
        }
    }
    // SAFETY: `list` came from `CredEnumerateW` and is not used afterwards.
    unsafe { CredFree(list as *const core::ffi::c_void) };

    if entries > 0 && newest.is_none() {
        warn!(
            entries,
            "Copilot CLI entries in Credential Manager hold no usable token"
        );
    }
    newest.map(|(_, token)| token)
}

/// # Safety
/// `text` must be null or point to a NUL-terminated UTF-16 string.
unsafe fn wide_to_string(text: *const u16) -> String {
    if text.is_null() {
        return String::new();
    }
    let mut len = 0;
    // SAFETY: the caller promises a NUL-terminated string.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` characters before the NUL were just read.
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) })
}

/// Where Copilot CLI is usually installed.
fn copilot_candidates(
    path_var: Option<&std::ffi::OsStr>,
    app_data: Option<&Path>,
    local_app_data: Option<&Path>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(path) = path_var {
        for dir in std::env::split_paths(path) {
            out.push(dir.join("copilot.exe"));
            out.push(dir.join("copilot.cmd"));
        }
    }
    if let Some(dir) = local_app_data {
        out.push(
            dir.join("Microsoft")
                .join("WinGet")
                .join("Links")
                .join("copilot.exe"),
        );
    }
    if let Some(dir) = app_data {
        out.push(dir.join("npm").join("copilot.cmd"));
    }
    out
}

/// The installed Copilot CLI, if any.
fn locate_copilot() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
    let path = std::env::var_os("PATH");
    copilot_candidates(
        path.as_deref(),
        var("APPDATA").as_deref(),
        var("LOCALAPPDATA").as_deref(),
    )
    .into_iter()
    .find(|candidate| candidate.is_file())
}

// ---------------------------------------------------------------------------
// GitHub CLI fallback
// ---------------------------------------------------------------------------

/// Places `gh.exe` can be, in the order to look: the PATH this process
/// started with, then the usual install folders. A program started before
/// `gh` was installed (or by Windows at sign-in) does not see the new PATH
/// entry, so the installer's own folders are checked too.
fn gh_candidates(
    path_var: Option<&std::ffi::OsStr>,
    local_app_data: Option<&Path>,
    program_files: &[PathBuf],
    user_profile: Option<&Path>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(path) = path_var {
        out.extend(std::env::split_paths(path).map(|dir| dir.join("gh.exe")));
    }
    for dir in program_files {
        out.push(dir.join("GitHub CLI").join("gh.exe"));
    }
    if let Some(dir) = local_app_data {
        out.push(dir.join("Programs").join("GitHub CLI").join("gh.exe"));
        out.push(
            dir.join("Microsoft")
                .join("WinGet")
                .join("Links")
                .join("gh.exe"),
        );
    }
    if let Some(dir) = user_profile {
        out.push(dir.join("scoop").join("shims").join("gh.exe"));
    }
    out
}

/// The GitHub CLI to run: the first existing candidate, else plain `gh`.
fn locate_gh() -> PathBuf {
    let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
    let program_files: Vec<_> = ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
        .iter()
        .filter_map(|name| var(name))
        .collect();
    let path = std::env::var_os("PATH");
    gh_candidates(
        path.as_deref(),
        var("LOCALAPPDATA").as_deref(),
        &program_files,
        var("USERPROFILE").as_deref(),
    )
    .into_iter()
    .find(|candidate| candidate.is_file())
    .unwrap_or_else(|| "gh".into())
}

fn gh_token() -> Result<String, CopilotAuthError> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new(locate_gh())
        .args(["auth", "token"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => CopilotAuthError::CliMissing,
            _ => CopilotAuthError::CommandFailed(error.to_string()),
        })?;
    if !out.status.success() {
        warn!(status = ?out.status.code(), "`gh auth token` failed");
    }
    parse_gh_token_output(out.status.success(), &out.stdout)
}

fn parse_gh_token_output(success: bool, stdout: &[u8]) -> Result<String, CopilotAuthError> {
    if !success {
        return Err(CopilotAuthError::NotAuthenticated);
    }
    let token = String::from_utf8_lossy(stdout).trim().to_string();
    if token.is_empty() {
        Err(CopilotAuthError::NotAuthenticated)
    } else {
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Made-up tokens of the right shape.
    const OAUTH: &str = "gho_abcdefghijklmnopqrstuvwxyz0123456789";
    const FINE_GRAINED: &str =
        "github_pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTU";

    #[test]
    fn a_bare_or_embedded_token_is_found() {
        assert_eq!(find_github_token(OAUTH).as_deref(), Some(OAUTH));
        assert_eq!(
            find_github_token(&format!("  {OAUTH}\r\n")).as_deref(),
            Some(OAUTH)
        );
        let json = format!(r#"{{"logged_in_users":[{{"login":"me"}}],"token":"{FINE_GRAINED}"}}"#);
        assert_eq!(find_github_token(&json).as_deref(), Some(FINE_GRAINED));
        // The first token in the text wins.
        let both = format!("{FINE_GRAINED} {OAUTH}");
        assert_eq!(find_github_token(&both).as_deref(), Some(FINE_GRAINED));
    }

    #[test]
    fn things_that_are_not_tokens_are_ignored() {
        assert_eq!(find_github_token(""), None);
        assert_eq!(find_github_token("gho_short"), None);
        assert_eq!(
            find_github_token("ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
            None,
            "classic tokens do not work with Copilot"
        );
        assert_eq!(
            find_github_token(r#"{"trusted_folders":["C:\\work"]}"#),
            None
        );
    }

    #[test]
    fn credential_blobs_are_read_as_utf8_or_utf16() {
        assert_eq!(token_from_blob(OAUTH.as_bytes()).as_deref(), Some(OAUTH));
        let wide: Vec<u8> = OAUTH.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(token_from_blob(&wide).as_deref(), Some(OAUTH));
        assert_eq!(token_from_blob(&[]), None);
    }

    #[test]
    fn copilot_cli_entries_are_recognised_by_name() {
        assert!(is_copilot_cli_entry("copilot-cli"));
        assert!(is_copilot_cli_entry("octocat.copilot-cli"));
        assert!(is_copilot_cli_entry(
            "Copilot-CLI/https://github.com:octocat"
        ));
        assert!(!is_copilot_cli_entry("git:https://github.com"));
        assert!(!is_copilot_cli_entry("OpenWritr/OpenAICompatibleApiKey"));
    }

    #[test]
    fn classic_tokens_in_variables_are_skipped() {
        assert_eq!(
            usable_variable(&format!(" {OAUTH} ")).as_deref(),
            Some(OAUTH)
        );
        assert_eq!(
            usable_variable("ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
            None
        );
        assert_eq!(usable_variable("   "), None);
    }

    #[test]
    fn copilot_cli_is_searched_on_path_then_in_winget_and_npm_folders() {
        let path = std::env::join_paths(["tools"]).unwrap();
        assert_eq!(
            copilot_candidates(
                Some(&path),
                Some(Path::new("roaming")),
                Some(Path::new("local"))
            ),
            [
                Path::new("tools").join("copilot.exe"),
                Path::new("tools").join("copilot.cmd"),
                Path::new("local")
                    .join("Microsoft")
                    .join("WinGet")
                    .join("Links")
                    .join("copilot.exe"),
                Path::new("roaming").join("npm").join("copilot.cmd"),
            ]
        );
        assert!(copilot_candidates(None, None, None).is_empty());
    }

    #[test]
    fn sources_are_described_without_the_token() {
        assert_eq!(TokenSource::CopilotCli.describe(), "GitHub Copilot CLI");
        assert_eq!(
            TokenSource::Environment("GH_TOKEN").describe(),
            "the GH_TOKEN environment variable"
        );
    }

    #[test]
    fn classifies_github_cli_token_results_without_exposing_tokens() {
        assert_eq!(
            parse_gh_token_output(false, b"").unwrap_err(),
            CopilotAuthError::NotAuthenticated
        );
        assert_eq!(
            parse_gh_token_output(true, b" \r\n").unwrap_err(),
            CopilotAuthError::NotAuthenticated
        );
        assert_eq!(
            parse_gh_token_output(true, b"secret-token\r\n").unwrap(),
            "secret-token"
        );
    }

    #[test]
    fn gh_is_searched_on_path_then_in_the_usual_install_folders() {
        let path = std::env::join_paths(["tools", "bin"]).unwrap();
        let found = gh_candidates(
            Some(&path),
            Some(Path::new("local")),
            &[PathBuf::from("programs")],
            Some(Path::new("home")),
        );
        assert_eq!(
            found,
            [
                Path::new("tools").join("gh.exe"),
                Path::new("bin").join("gh.exe"),
                Path::new("programs").join("GitHub CLI").join("gh.exe"),
                Path::new("local")
                    .join("Programs")
                    .join("GitHub CLI")
                    .join("gh.exe"),
                Path::new("local")
                    .join("Microsoft")
                    .join("WinGet")
                    .join("Links")
                    .join("gh.exe"),
                Path::new("home").join("scoop").join("shims").join("gh.exe"),
            ]
        );
        assert!(gh_candidates(None, None, &[], None).is_empty());
    }
}
