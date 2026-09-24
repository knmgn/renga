//! GitHub Copilot CLI integration that lives outside the MCP protocol:
//! its lifecycle hooks and its folder-trust list.
//!
//! Both are files under Copilot's config directory (`~/.copilot`, or
//! `$COPILOT_HOME`), and every format fact below was checked against a
//! live Copilot 1.0.82 rather than taken from documentation:
//!
//! - **Hooks.** Copilot loads every `*.json` under `<home>/hooks/` as a
//!   user-level hook file and runs each hook with the agent's own
//!   environment, so `RENGA_PANE_ID` / `RENGA_SOCKET` / `RENGA_TOKEN`
//!   reach the hook command unchanged. renga owns exactly one file there,
//!   [`HOOKS_FILE_NAME`], so installing and removing it never touches a
//!   hook the user wrote.
//! - **Folder trust.** Copilot asks "Do you trust the files in this
//!   folder?" on the first launch in a directory, and renga refuses to
//!   type into that dialog, so an unattended worker spawned there stalls
//!   until a human answers. Choosing "remember" appends the directory to
//!   `trustedFolders` in `<home>/config.json`, and a directory *under* a
//!   trusted one is trusted too. That file is JSON with a `//` comment
//!   header, not plain JSON — see [`split_comment_header`].

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};

use crate::ipc::{client, endpoint, PeerClientKind, Request};

/// The one file under `<copilot home>/hooks/` that renga owns.
pub(crate) const HOOKS_FILE_NAME: &str = "renga.json";

/// Hidden subcommand Copilot runs for each hook (see
/// [`crate::cli::IpcCommand::CopilotHook`]).
pub(crate) const HOOK_SUBCOMMAND: &str = "copilot-hook";

/// The Copilot events renga subscribes to, in Copilot's native
/// camelCase spelling. Each one moves the server-side state
/// ([`crate::ipc::AgentActivity::from_hook`]); nothing is subscribed
/// "just in case", because every hook is a process spawn on the agent's
/// critical path.
///
/// Deliberately absent: `preToolUse` and `permissionRequest`, whose hook
/// *output* is a permission decision. A renga hook that errored or timed
/// out there could deny the agent's tool call; the events below have no
/// decision to get wrong.
pub(crate) const HOOK_EVENTS: &[&str] = &[
    "userPromptSubmitted",
    "postToolUse",
    "postToolUseFailure",
    "agentStop",
    "notification",
    "errorOccurred",
    "sessionEnd",
];

/// Copilot kills a hook after this long and carries on. The hook is one
/// local IPC round-trip, so this is a ceiling for a wedged renga, not a
/// budget anything is expected to use.
const HOOK_TIMEOUT_SEC: u64 = 5;

/// How long the hook waits for renga to answer. Well inside
/// [`HOOK_TIMEOUT_SEC`]: Copilot kills a hook that runs past that and
/// shows the user a hook failure, so a wedged renga must cost a quiet
/// give-up here instead — the report is advisory and loses nothing a
/// later event will not restate.
const HOOK_IPC_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// How long the hook waits for Copilot to finish writing its payload.
/// Copilot writes it and closes the pipe immediately; this only bounds a
/// caller that holds stdin open.
const HOOK_STDIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);

/// Cap on how much of the hook payload is kept for parsing. The fields
/// renga reads are tiny; a `userPromptSubmitted` payload carries the
/// whole prompt, which is read and discarded rather than buffered.
const HOOK_PAYLOAD_CAP: u64 = 64 * 1024;

/// Copilot's config directory: `$COPILOT_HOME`, else `~/.copilot`.
pub(crate) fn copilot_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("COPILOT_HOME").filter(|h| !h.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow!("could not resolve the current user's home directory"))?;
    Ok(home.join(".copilot"))
}

pub(crate) fn hooks_file_path() -> Result<PathBuf> {
    Ok(copilot_home()?.join("hooks").join(HOOKS_FILE_NAME))
}

fn config_json_path() -> Result<PathBuf> {
    Ok(copilot_home()?.join("config.json"))
}

// ── hook subcommand ─────────────────────────────────────────────

/// Entry point for `renga-cp copilot-hook <event>`.
///
/// Always succeeds. Copilot surfaces a failing hook in the user's
/// session, and this hook is installed user-wide: it also fires in every
/// Copilot session outside renga, where there is simply nobody to tell.
/// So a missing `RENGA_*` environment, an unreachable renga, or a server
/// too old to know the request all end the same way — exit 0, no output.
pub fn run_hook(event: &str) -> Result<()> {
    let _ = report_hook(event);
    Ok(())
}

fn report_hook(event: &str) -> Result<()> {
    // First, so every exit below — including "not inside renga" — has
    // consumed what Copilot wrote to us.
    let payload = read_hook_payload_with_deadline();
    let Some(pane_id) = std::env::var("RENGA_PANE_ID")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    else {
        return Ok(());
    };
    let endpoint = endpoint::endpoint_from_env()?;
    let (notification_type, recoverable) = hook_payload_details(&payload);
    client::send_request_with_timeout(
        &endpoint,
        &Request::AgentHook {
            pane_id,
            kind: PeerClientKind::Copilot,
            event: event.to_string(),
            notification_type,
            recoverable,
        },
        HOOK_IPC_TIMEOUT,
    )?;
    Ok(())
}

/// [`read_hook_payload`] on stdin, abandoned after
/// [`HOOK_STDIN_TIMEOUT`]. The reader thread is left behind on timeout;
/// the process exits moments later and takes it along.
fn read_hook_payload_with_deadline() -> Vec<u8> {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("copilot-hook-stdin".into())
        .spawn(move || {
            let _ = tx.send(read_hook_payload(std::io::stdin().lock()));
        });
    if spawned.is_err() {
        return Vec::new();
    }
    rx.recv_timeout(HOOK_STDIN_TIMEOUT).unwrap_or_default()
}

/// Read the hook's stdin to the end, keeping at most
/// [`HOOK_PAYLOAD_CAP`] bytes. Draining the rest (instead of exiting
/// with it unread) keeps Copilot's write side from seeing a broken pipe.
fn read_hook_payload(mut input: impl Read) -> Vec<u8> {
    let mut kept = Vec::new();
    let _ = (&mut input).take(HOOK_PAYLOAD_CAP).read_to_end(&mut kept);
    let _ = std::io::copy(&mut input, &mut std::io::sink());
    kept
}

/// Pull the two verdict-relevant fields out of a hook payload. Copilot
/// sends camelCase keys for camelCase event names and snake_case ones
/// for its PascalCase aliases; both are accepted. Anything unparseable
/// yields `(None, None)` — the event name alone still counts.
fn hook_payload_details(payload: &[u8]) -> (Option<String>, Option<bool>) {
    let Ok(value) = serde_json::from_slice::<Value>(payload) else {
        return (None, None);
    };
    let pick = |keys: &[&str]| keys.iter().find_map(|k| value.get(*k));
    let notification_type = pick(&["notificationType", "notification_type"])
        .and_then(Value::as_str)
        .map(str::to_string);
    let recoverable = pick(&["recoverable"]).and_then(Value::as_bool);
    (notification_type, recoverable)
}

// ── hooks file ──────────────────────────────────────────────────

/// Whether renga's hooks file is present and points at this binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HooksStatus {
    Missing,
    Current,
    /// Present but not what [`hooks_file_contents`] would write now —
    /// typically a renga binary that has since moved.
    Stale,
}

/// The exact contents renga writes to its hooks file for `exe`.
pub(crate) fn hooks_file_contents(exe: &Path) -> Result<String> {
    let exe_str = exe.to_str().ok_or_else(|| {
        anyhow!(
            "renga binary path is not valid UTF-8 ({}); cannot write it into a Copilot hook \
             command",
            exe.display()
        )
    })?;
    let mut hooks = Map::new();
    for event in HOOK_EVENTS {
        hooks.insert(
            (*event).to_string(),
            json!([{
                "type": "command",
                "bash": format!("{} {HOOK_SUBCOMMAND} {event}", posix_quote(exe_str)),
                "powershell": format!("& {} {HOOK_SUBCOMMAND} {event}", powershell_quote(exe_str)),
                "timeoutSec": HOOK_TIMEOUT_SEC,
            }]),
        );
    }
    let doc = json!({ "version": 1, "hooks": Value::Object(hooks) });
    Ok(format!("{}\n", serde_json::to_string_pretty(&doc)?))
}

pub(crate) fn hooks_status(exe: &Path) -> Result<HooksStatus> {
    let path = hooks_file_path()?;
    match fs::read_to_string(&path) {
        Ok(existing) if existing == hooks_file_contents(exe)? => Ok(HooksStatus::Current),
        Ok(_) => Ok(HooksStatus::Stale),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HooksStatus::Missing),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Write renga's hooks file for `exe`. Idempotent.
pub(crate) fn install_hooks(exe: &Path) -> Result<PathBuf> {
    let path = hooks_file_path()?;
    let contents = hooks_file_contents(exe)?;
    if fs::read_to_string(&path).ok().as_deref() == Some(contents.as_str()) {
        return Ok(path);
    }
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("hooks path {} has no parent", path.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    write_atomically(&path, contents.as_bytes(), None)?;
    Ok(path)
}

/// Remove renga's hooks file. Returns whether there was one.
pub(crate) fn uninstall_hooks() -> Result<bool> {
    let path = hooks_file_path()?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

fn posix_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn powershell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// ── folder trust ────────────────────────────────────────────────

/// Split Copilot's `config.json` into its leading comment header and the
/// JSON document after it.
///
/// Copilot 1.0.82 writes the file as
///
/// ```text
/// // User settings belong in settings.json.
/// // This file is managed automatically.
/// {
///   ...
/// ```
///
/// which a strict JSON parser rejects on the first byte. Only a header
/// of whole `//` lines (and blank lines) is understood; a comment
/// anywhere else leaves it inside the body, whose parse then fails and
/// makes renga leave the file alone rather than rewrite what it cannot
/// round-trip.
fn split_comment_header(src: &str) -> (&str, &str) {
    let mut end = 0;
    for line in src.split_inclusive('\n') {
        let t = line.trim();
        if t.is_empty() || t.starts_with("//") {
            end += line.len();
        } else {
            break;
        }
    }
    src.split_at(end)
}

fn parse_config(src: &str) -> Result<(&str, Map<String, Value>)> {
    let (header, body) = split_comment_header(src);
    match serde_json::from_str::<Value>(body) {
        Ok(Value::Object(map)) => Ok((header, map)),
        Ok(_) => bail!("Copilot config.json is not a JSON object"),
        Err(e) => bail!("Copilot config.json is not in a shape renga can edit safely: {e}"),
    }
}

/// The `trustedFolders` entries in Copilot's config source.
fn trusted_folders_in(src: &str) -> Result<Vec<PathBuf>> {
    let (_, map) = parse_config(src)?;
    Ok(map
        .get("trustedFolders")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default())
}

/// `src` with `folder` appended to `trustedFolders`, header preserved.
/// Every other key survives; only its order may change, since renga's
/// JSON map is sorted. Copilot reads the file by key.
fn with_trusted_folder(src: &str, folder: &str) -> Result<String> {
    let (header, mut map) = parse_config(src)?;
    let list = map
        .entry("trustedFolders")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(items) = list else {
        bail!("Copilot config.json has a `trustedFolders` that is not a list");
    };
    items.push(Value::String(folder.to_string()));
    Ok(format!(
        "{header}{}\n",
        serde_json::to_string_pretty(&Value::Object(map))?
    ))
}

/// Resolve symlinks the way Copilot does before comparing folders, but
/// fall back to the path as given when it does not exist (yet).
fn canonical(path: &Path) -> PathBuf {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    strip_verbatim_prefix(resolved)
}

/// Windows `canonicalize` returns `\\?\C:\…`, which Copilot would not
/// recognize as the folder it has on record.
fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(rest) = p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
            if !rest.starts_with("UNC\\") {
                return PathBuf::from(rest);
            }
        }
    }
    p
}

/// Whether `path` is one of `trusted` or lies under one of them.
fn is_under_trusted(trusted: &[PathBuf], path: &Path) -> bool {
    let path = canonical(path);
    trusted.iter().any(|t| path.starts_with(canonical(t)))
}

/// Whether trusting `path` would trust far more than a project: the
/// filesystem root, the user's home directory, or any folder above it
/// (`/home`, `C:\Users`), under which Copilot would then trust
/// everything — other users' homes included.
pub(crate) fn too_broad_to_trust(path: &Path) -> bool {
    let path = canonical(path);
    path.parent().is_none()
        || dirs::home_dir().is_some_and(|home| canonical(&home).starts_with(&path))
}

/// Environment variables that point `git` at a repository regardless of
/// `-C`. Inherited from whatever launched the agent, any of them would
/// make every folder answer with the same repository.
const GIT_LOCATION_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    // `-c`-style config injected through the environment can set
    // `core.worktree` and move what `--show-toplevel` answers.
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
];

/// `git -C <dir> rev-parse --path-format=absolute <what>`, canonicalized,
/// or `None` outside a repository or when `git` cannot answer.
fn git_rev_parse_path(dir: &Path, what: &str) -> Option<PathBuf> {
    let mut cmd = std::process::Command::new("git");
    for var in GIT_LOCATION_ENV {
        cmd.env_remove(var);
    }
    let out = cmd
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--path-format=absolute", what])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| canonical(Path::new(s)))
}

/// Whether `a` and `b` are in the same git repository: every worktree of
/// one repository points at the same shared git dir.
fn same_git_repository(a: &Path, b: &Path) -> bool {
    match (
        git_rev_parse_path(a, "--git-common-dir"),
        git_rev_parse_path(b, "--git-common-dir"),
    ) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Whether `new_folder` may inherit trust from `caller_folder`, by the
/// list in `trusted`.
///
/// Copilot's own rule, and only that: when it creates a worktree of a
/// repository the user trusted, the worktree is trusted too. So the
/// *whole* repository the caller works in must already be trusted — an
/// entry at or above the caller's worktree root, not merely on some
/// subfolder of it — and `new_folder` must belong to that repository. A
/// trusted `repo/sub` therefore never spreads to `repo/`, and a home
/// directory kept in git never lends trust to an arbitrary folder
/// beneath it, because the home itself is not trusted.
fn may_inherit_trust_from(trusted: &[PathBuf], caller_folder: &Path, new_folder: &Path) -> bool {
    let Some(caller_root) = git_rev_parse_path(caller_folder, "--show-toplevel") else {
        return false;
    };
    is_under_trusted(trusted, &caller_root) && same_git_repository(caller_folder, new_folder)
}

/// [`may_inherit_trust_from`] against Copilot's trust list on disk.
pub(crate) fn may_inherit_trust(caller_folder: &Path, new_folder: &Path) -> Result<bool> {
    let cfg = config_json_path()?;
    let trusted = match fs::read_to_string(&cfg) {
        Ok(src) => trusted_folders_in(&src)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("read {}", cfg.display())),
    };
    Ok(may_inherit_trust_from(&trusted, caller_folder, new_folder))
}

/// Whether Copilot will treat `path` as trusted, read from disk. A
/// missing config file means nothing is trusted yet.
pub(crate) fn folder_is_trusted(path: &Path) -> Result<bool> {
    let cfg = config_json_path()?;
    match fs::read_to_string(&cfg) {
        Ok(src) => Ok(is_under_trusted(&trusted_folders_in(&src)?, path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("read {}", cfg.display())),
    }
}

/// Add `path` to Copilot's trusted folders, unless it is already covered.
///
/// `config.json` also holds Copilot's login tokens and is rewritten by
/// every running Copilot, so the write is careful in three ways: it
/// keeps the file's permissions, it lands by rename so no reader sees a
/// half-written file, and it re-reads the file just before the rename
/// and aborts if Copilot changed it in between — clobbering a token
/// refresh would log the user out, which is far worse than one more
/// trust prompt.
pub(crate) fn trust_folder(path: &Path) -> Result<()> {
    trust_folder_in(&config_json_path()?, path)
}

fn trust_folder_in(cfg: &Path, path: &Path) -> Result<()> {
    // Write through a symlinked config.json, not over it — and not at
    // all through a dangling one, which renaming over would replace.
    let is_link = fs::symlink_metadata(cfg).is_ok_and(|m| m.file_type().is_symlink());
    let cfg = &match fs::canonicalize(cfg) {
        Ok(real) => real,
        Err(_) if is_link => bail!("{} is a dangling symlink; left it alone", cfg.display()),
        Err(_) => cfg.to_path_buf(),
    };
    let folder = canonical(path);
    let folder_str = folder
        .to_str()
        .ok_or_else(|| anyhow!("folder path {} is not valid UTF-8", folder.display()))?;
    let original = match fs::read_to_string(cfg) {
        Ok(src) => Some(src),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("read {}", cfg.display())),
    };
    let src = original.as_deref().unwrap_or("{}");
    if is_under_trusted(&trusted_folders_in(src)?, &folder) {
        return Ok(());
    }
    let updated = with_trusted_folder(src, folder_str)?;
    if let Some(dir) = cfg.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    write_atomically(
        cfg,
        updated.as_bytes(),
        Some(original.as_deref().map_or(Expect::Absent, Expect::Content)),
    )
}

/// What `path` must still hold right before [`write_atomically`]
/// renames over it.
#[derive(Debug, Clone, Copy)]
enum Expect<'a> {
    /// Exactly this content.
    Content(&'a str),
    /// Nothing: the file did not exist when it was read.
    Absent,
}

/// Write `contents` to `path` via a sibling temp file and a rename.
///
/// With `expect = Some(..)`, the rename only happens if `path` is still
/// in the expected state right before it. The temp file takes the
/// existing file's permissions (a fresh file gets owner-only on Unix):
/// created with the default umask it would briefly — then permanently —
/// expose Copilot's tokens to other local users.
fn write_atomically(path: &Path, contents: &[u8], expect: Option<Expect<'_>>) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("bad file path {}", path.display()))?;
    let tmp = path.with_file_name(format!(".{file_name}.renga-{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut f = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        match fs::metadata(path) {
            Ok(meta) => fs::set_permissions(&tmp, meta.permissions())?,
            Err(_) => restrict_to_owner(&tmp)?,
        }
        f.write_all(contents)?;
        f.sync_all()?;
        drop(f);
        let unchanged = match expect {
            None => true,
            Some(Expect::Content(prev)) => fs::read_to_string(path).ok().as_deref() == Some(prev),
            Some(Expect::Absent) => !path.exists(),
        };
        if !unchanged {
            bail!(
                "{} changed while renga was updating it (Copilot rewrote it); left it as is",
                path.display()
            );
        }
        fs::rename(&tmp, path)
            .with_context(|| format!("replace {} with {}", path.display(), tmp.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim shape of a Copilot 1.0.82 config.json, tokens elided.
    const LIVE_CONFIG: &str = "// User settings belong in settings.json.\n\
        // This file is managed automatically.\n\
        {\n  \"firstLaunchAt\": \"2026-08-23T14:52:05.111Z\",\n  \"loggedInUsers\": [\n    {\n      \
        \"host\": \"https://github.com\",\n      \"login\": \"someone\"\n    }\n  ],\n  \
        \"trustedFolders\": [\n    \"/home/someone/repo\"\n  ]\n}\n";

    #[test]
    fn the_comment_header_is_split_off_and_kept() {
        let (header, body) = split_comment_header(LIVE_CONFIG);
        assert_eq!(
            header,
            "// User settings belong in settings.json.\n// This file is managed automatically.\n"
        );
        assert!(body.starts_with('{'));
        assert!(
            serde_json::from_str::<Value>(LIVE_CONFIG).is_err(),
            "the premise: plain JSON parsing rejects Copilot's file outright"
        );
    }

    #[test]
    fn trusted_folders_are_read_through_the_header() {
        assert_eq!(
            trusted_folders_in(LIVE_CONFIG).unwrap(),
            vec![PathBuf::from("/home/someone/repo")]
        );
        assert!(trusted_folders_in("{}").unwrap().is_empty());
    }

    #[test]
    fn adding_a_folder_keeps_the_header_and_every_other_key() {
        let out = with_trusted_folder(LIVE_CONFIG, "/tmp/work").unwrap();
        assert!(out.starts_with("// User settings belong in settings.json.\n"));
        assert_eq!(
            trusted_folders_in(&out).unwrap(),
            vec![
                PathBuf::from("/home/someone/repo"),
                PathBuf::from("/tmp/work")
            ]
        );
        let (_, map) = parse_config(&out).unwrap();
        assert_eq!(map["loggedInUsers"][0]["login"], "someone");
        assert_eq!(map["firstLaunchAt"], "2026-08-23T14:52:05.111Z");
    }

    #[test]
    fn a_config_without_the_key_gains_it() {
        let out = with_trusted_folder("{\"a\": 1}", "/tmp/work").unwrap();
        assert_eq!(
            trusted_folders_in(&out).unwrap(),
            vec![PathBuf::from("/tmp/work")]
        );
    }

    /// renga rewrites a file holding login tokens only when it can
    /// round-trip it; anything else is left for Copilot to manage.
    #[test]
    fn a_config_renga_cannot_round_trip_is_refused() {
        let inline_comment = "{\n  \"a\": 1 // trailing\n}\n";
        assert!(with_trusted_folder(inline_comment, "/x").is_err());
        assert!(with_trusted_folder("[]", "/x").is_err());
        assert!(with_trusted_folder("{\"trustedFolders\": \"/x\"}", "/y").is_err());
    }

    #[test]
    fn subdirectories_of_a_trusted_folder_count_as_trusted() {
        let trusted = vec![PathBuf::from("/nonexistent-renga-test/repo")];
        assert!(is_under_trusted(
            &trusted,
            Path::new("/nonexistent-renga-test/repo")
        ));
        assert!(is_under_trusted(
            &trusted,
            Path::new("/nonexistent-renga-test/repo/sub/dir")
        ));
        assert!(
            !is_under_trusted(&trusted, Path::new("/nonexistent-renga-test/repo-other")),
            "a shared name prefix is not containment"
        );
        assert!(!is_under_trusted(
            &trusted,
            Path::new("/nonexistent-renga-test")
        ));
    }

    /// A scratch directory under the system temp dir, removed on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "renga-copilot-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Scratch(canonical(&dir))
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn trusting_a_folder_edits_the_config_in_place_and_keeps_its_mode() {
        let home = Scratch::new("home");
        let cfg = home.0.join("config.json");
        fs::write(&cfg, LIVE_CONFIG).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&cfg, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let work = home.0.join("work");
        fs::create_dir(&work).unwrap();

        trust_folder_in(&cfg, &work).unwrap();
        let out = fs::read_to_string(&cfg).unwrap();
        assert!(out.starts_with("// User settings belong in settings.json.\n"));
        assert!(trusted_folders_in(&out).unwrap().contains(&work));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&cfg).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        // Idempotent, including for a folder beneath a trusted one.
        let sub = work.join("sub");
        fs::create_dir(&sub).unwrap();
        trust_folder_in(&cfg, &sub).unwrap();
        assert_eq!(fs::read_to_string(&cfg).unwrap(), out);

        let leftovers: Vec<_> = fs::read_dir(&home.0)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn a_missing_config_is_created_owner_only() {
        let home = Scratch::new("fresh");
        let cfg = home.0.join("config.json");
        trust_folder_in(&cfg, &home.0).unwrap();
        assert_eq!(
            trusted_folders_in(&fs::read_to_string(&cfg).unwrap()).unwrap(),
            vec![home.0.clone()]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&cfg).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    /// The race guard: if Copilot rewrote the file after renga read it,
    /// renga's stale edit must not land — it could drop a token refresh.
    #[test]
    fn a_config_that_changed_underneath_is_left_alone() {
        let home = Scratch::new("race");
        let cfg = home.0.join("config.json");
        fs::write(&cfg, "{\"copilotTokens\": \"new\"}").unwrap();
        let err = write_atomically(&cfg, b"{}", Some(Expect::Content("{}"))).unwrap_err();
        assert!(err.to_string().contains("changed while renga"), "{err:#}");
        assert_eq!(
            fs::read_to_string(&cfg).unwrap(),
            "{\"copilotTokens\": \"new\"}"
        );
        let err = write_atomically(&cfg, b"{}", Some(Expect::Absent)).unwrap_err();
        assert!(err.to_string().contains("changed while renga"), "{err:#}");
        assert_eq!(
            fs::read_dir(&home.0).unwrap().count(),
            1,
            "temp file removed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_config_symlink_is_left_alone() {
        let home = Scratch::new("dangling");
        let cfg = home.0.join("config.json");
        std::os::unix::fs::symlink(home.0.join("missing.json"), &cfg).unwrap();
        assert!(trust_folder_in(&cfg, &home.0).is_err());
        assert!(fs::symlink_metadata(&cfg).unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_is_written_through_not_replaced() {
        let home = Scratch::new("link");
        let real = home.0.join("real.json");
        fs::write(&real, "{}").unwrap();
        let cfg = home.0.join("config.json");
        std::os::unix::fs::symlink(&real, &cfg).unwrap();
        trust_folder_in(&cfg, &home.0).unwrap();
        assert!(fs::symlink_metadata(&cfg).unwrap().file_type().is_symlink());
        assert_eq!(
            trusted_folders_in(&fs::read_to_string(&real).unwrap()).unwrap(),
            vec![home.0.clone()]
        );
    }

    #[test]
    fn the_root_and_home_are_too_broad_to_trust() {
        let root = std::env::current_dir()
            .unwrap()
            .ancestors()
            .last()
            .unwrap()
            .to_path_buf();
        assert!(too_broad_to_trust(&root));
        if let Some(home) = dirs::home_dir() {
            assert!(too_broad_to_trust(&home));
            if let Some(above) = home.parent() {
                assert!(
                    too_broad_to_trust(above),
                    "{} holds every home",
                    above.display()
                );
            }
            assert!(!too_broad_to_trust(&home.join("some-project")));
        }
    }

    /// Serializes the git tests: one of them sets `GIT_DIR` process-wide.
    static GIT_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// `root/main` (a repository with one commit), `root/wt` (a worktree
    /// of it), `root/other` (an unrelated repository). `None` without git.
    fn repo_fixture(root: &Path) -> Option<(PathBuf, PathBuf, PathBuf)> {
        let main = root.join("main");
        fs::create_dir(&main).unwrap();
        if !git(&main, &["init", "-q"]) {
            eprintln!("git unavailable; skipping");
            return None;
        }
        assert!(git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]));
        fs::create_dir(main.join("sub")).unwrap();
        let wt = root.join("wt");
        assert!(git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]));
        let other = root.join("other");
        fs::create_dir(&other).unwrap();
        assert!(git(&other, &["init", "-q"]));
        Some((main, wt, other))
    }

    #[test]
    fn trust_is_inherited_only_from_a_wholly_trusted_repository() {
        let _g = GIT_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let root = Scratch::new("git");
        let Some((main, wt, other)) = repo_fixture(&root.0) else {
            return;
        };
        let sub = main.join("sub");

        // The whole repository is trusted: its worktree inherits, an
        // unrelated repository does not.
        let whole = vec![main.clone()];
        assert!(may_inherit_trust_from(&whole, &main, &wt));
        assert!(may_inherit_trust_from(&whole, &sub, &wt));
        assert!(!may_inherit_trust_from(&whole, &main, &other));

        // Only a subfolder is trusted: nothing spreads from it — not up
        // to the repository root, not across to a worktree.
        let partial = vec![sub.clone()];
        assert!(!may_inherit_trust_from(&partial, &sub, &main));
        assert!(!may_inherit_trust_from(&partial, &sub, &wt));
    }

    /// A home directory kept in git: `notes` is trusted, the home is not,
    /// so a sibling folder in the same repository must not inherit.
    #[test]
    fn a_trusted_folder_inside_a_home_repository_lends_nothing_to_its_siblings() {
        let _g = GIT_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let home = Scratch::new("dotfiles");
        if !git(&home.0, &["init", "-q"]) {
            return;
        }
        let notes = home.0.join("notes");
        let downloads = home.0.join("Downloads");
        fs::create_dir(&notes).unwrap();
        fs::create_dir(&downloads).unwrap();
        assert!(same_git_repository(&notes, &downloads), "premise");
        assert!(!may_inherit_trust_from(
            std::slice::from_ref(&notes),
            &notes,
            &downloads
        ));
    }

    /// An inherited `GIT_DIR` would make every folder report the same
    /// repository; renga's git probe must not see it.
    #[test]
    fn an_inherited_git_dir_does_not_make_folders_match() {
        let _g = GIT_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let root = Scratch::new("gitdir");
        let Some((main, _wt, other)) = repo_fixture(&root.0) else {
            return;
        };
        let plain = root.0.join("plain");
        fs::create_dir(&plain).unwrap();
        std::env::set_var("GIT_DIR", main.join(".git"));
        let matched = same_git_repository(&main, &plain) || same_git_repository(&main, &other);
        let inherited = may_inherit_trust_from(std::slice::from_ref(&main), &main, &plain);
        std::env::remove_var("GIT_DIR");
        assert!(!matched, "GIT_DIR leaked into the git probe");
        assert!(!inherited);
    }

    #[test]
    fn the_hooks_file_runs_this_binary_for_every_subscribed_event() {
        let out = hooks_file_contents(Path::new("/opt/it's here/renga-cp")).unwrap();
        let doc: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(doc["version"], 1);
        let hooks = doc["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), HOOK_EVENTS.len());
        let stop = &hooks["agentStop"][0];
        assert_eq!(stop["type"], "command");
        assert_eq!(
            stop["bash"],
            "'/opt/it'\\''s here/renga-cp' copilot-hook agentStop"
        );
        assert_eq!(
            stop["powershell"],
            "& '/opt/it''s here/renga-cp' copilot-hook agentStop"
        );
        assert_eq!(stop["timeoutSec"], HOOK_TIMEOUT_SEC);
    }

    /// Every event renga installs must be one the server acts on;
    /// a subscribed-but-ignored event is a process spawn per occurrence
    /// for nothing.
    #[test]
    fn every_installed_event_moves_the_server_state() {
        use crate::ipc::{AgentActivity, AgentHookEffect};
        for event in HOOK_EVENTS {
            let effect = AgentActivity::from_hook(
                PeerClientKind::Copilot,
                event,
                Some("permission_prompt"),
                None,
            );
            assert_ne!(effect, AgentHookEffect::Ignore, "{event} is ignored");
        }
    }

    #[test]
    fn payload_details_accept_both_key_spellings() {
        assert_eq!(
            hook_payload_details(br#"{"sessionId":"s","notificationType":"permission_prompt"}"#),
            (Some("permission_prompt".to_string()), None)
        );
        assert_eq!(
            hook_payload_details(br#"{"notification_type":"elicitation_dialog"}"#),
            (Some("elicitation_dialog".to_string()), None)
        );
        assert_eq!(
            hook_payload_details(br#"{"recoverable":false}"#),
            (None, Some(false))
        );
        assert_eq!(hook_payload_details(b"not json"), (None, None));
    }

    #[test]
    fn an_oversized_payload_is_drained_but_not_kept() {
        let big = vec![b'x'; (HOOK_PAYLOAD_CAP as usize) * 2];
        let mut cursor = std::io::Cursor::new(big);
        let kept = read_hook_payload(&mut cursor);
        assert_eq!(kept.len(), HOOK_PAYLOAD_CAP as usize);
        assert_eq!(
            cursor.position(),
            HOOK_PAYLOAD_CAP * 2,
            "the rest is consumed"
        );
    }
}
