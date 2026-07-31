use crate::backups::{action_backup_root, BackupMeta};
use crate::error::{CodexxError, Result};
use crate::file_io::{ensure_directory, io_err, json_err, parse_toml_document, write_private_json};
use crate::paths::app_home;
use crate::{auth_path, config_path, string_value};
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

const SNAPSHOT_VERSION: u32 = 2;
const LEGACY_SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub(crate) struct OfficialConfigCandidate {
    pub(crate) auth: Value,
    pub(crate) model: Option<String>,
    pub(crate) source: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OfficialConfigDraft {
    auth_json: String,
    model: Option<String>,
    source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OfficialConfigSnapshot {
    version: u32,
    codex_dir: String,
    captured_at: String,
    model: Option<String>,
    #[serde(default)]
    auth: Option<Value>,
}

enum SnapshotState {
    Missing,
    Reset,
    Ready(OfficialConfigCandidate),
}

fn canonical_identity(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

pub(crate) fn official_snapshot_path(codex_dir: &Path) -> Result<PathBuf> {
    let identity = canonical_identity(codex_dir);
    let digest = Sha256::digest(identity.as_bytes());
    Ok(app_home()?
        .join("official-configs")
        .join(format!("{digest:x}.json")))
}

fn value_has_material(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(value) => !value.trim().is_empty(),
        Value::Array(values) => values.iter().any(value_has_material),
        Value::Object(values) => values.values().any(value_has_material),
        Value::Bool(_) | Value::Number(_) => true,
    }
}

pub(crate) fn auth_value_has_material(value: &Value) -> bool {
    value.as_object().is_some_and(|auth| {
        auth.iter()
            .filter(|(key, _)| key.as_str() != "auth_mode")
            .any(|(_, value)| value_has_material(value))
    })
}

fn is_chatgpt_auth(value: &Value) -> bool {
    let chatgpt_mode = value
        .get("auth_mode")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode.eq_ignore_ascii_case("chatgpt"));
    let has_api_key = value
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .is_some_and(|key| !key.trim().is_empty());
    chatgpt_mode
        && !has_api_key
        && value
            .get("tokens")
            .and_then(Value::as_object)
            .is_some_and(|tokens| {
                ["access_token", "refresh_token", "id_token"]
                    .iter()
                    .any(|key| {
                        tokens
                            .get(*key)
                            .and_then(Value::as_str)
                            .is_some_and(|value| !value.trim().is_empty())
                    })
            })
}

fn has_openai_api_key(value: &Value) -> bool {
    value
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .is_some_and(|key| !key.trim().is_empty())
}

fn read_auth_value(path: &Path) -> Result<Option<Value>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(path).map_err(|error| io_err(path, error))?;
    let value: Value = serde_json::from_str(&text).map_err(|error| json_err(path, error))?;
    if !value.is_object() || !auth_value_has_material(&value) {
        return Ok(None);
    }
    Ok(Some(value))
}

fn official_model(codex_dir: &Path) -> Result<Option<String>> {
    let path = config_path(codex_dir);
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).map_err(|error| io_err(&path, error))?;
    let doc = parse_toml_document(&path, &text)?;
    Ok(string_value(&doc, "model"))
}

pub(crate) fn live_config_is_official(codex_dir: &Path) -> Result<bool> {
    let path = config_path(codex_dir);
    if !path.is_file() {
        return Ok(true);
    }
    let text = fs::read_to_string(&path).map_err(|error| io_err(&path, error))?;
    let doc = parse_toml_document(&path, &text)?;
    Ok(document_is_official(&doc))
}

pub(crate) fn document_is_official(doc: &toml_edit::DocumentMut) -> bool {
    let Some(provider) = string_value(doc, "model_provider") else {
        return true;
    };
    if provider.eq_ignore_ascii_case("openai") {
        return true;
    }
    if provider != "custom" {
        return false;
    }
    doc.get("model_providers")
        .and_then(|item| item.as_table())
        .and_then(|providers| providers.get("custom"))
        .and_then(|item| item.as_table())
        .is_some_and(|table| {
            let has_no_endpoint = table
                .get("base_url")
                .and_then(|item| item.as_str())
                .is_none_or(|value| value.trim().is_empty());
            let is_openai = table
                .get("name")
                .and_then(|item| item.as_str())
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("openai"));
            has_no_endpoint
                && is_openai
                && table
                    .get("requires_openai_auth")
                    .and_then(|item| item.as_bool())
                    == Some(true)
        })
}

fn write_snapshot(codex_dir: &Path, model: Option<String>, auth: Option<Value>) -> Result<()> {
    let path = official_snapshot_path(codex_dir)?;
    if let Some(parent) = path.parent() {
        ensure_directory(parent)?;
    }
    let snapshot = OfficialConfigSnapshot {
        version: SNAPSHOT_VERSION,
        codex_dir: canonical_identity(codex_dir),
        captured_at: Local::now().to_rfc3339(),
        model,
        auth,
    };
    let value = serde_json::to_value(snapshot)
        .map_err(|error| CodexxError::Config(format!("序列化官方配置快照失败: {error}")))?;
    write_private_json(&path, &value)
}

pub(crate) fn save_official_config_snapshot(
    codex_dir: &Path,
    model: Option<String>,
    auth: &Value,
) -> Result<()> {
    if !auth.is_object() || !auth_value_has_material(auth) {
        return Err(CodexxError::Config(
            "官方 auth.json 没有可用认证信息，请先完成官方登录".to_string(),
        ));
    }
    write_snapshot(codex_dir, model, Some(auth.clone()))
}

pub(crate) fn mark_official_config_reset(codex_dir: &Path, model: Option<String>) -> Result<()> {
    write_snapshot(codex_dir, model, None)
}

pub(crate) fn capture_live_official_config_before_provider_switch(
    codex_dir: &Path,
) -> Result<bool> {
    capture_live_official_auth(codex_dir, |auth| {
        is_chatgpt_auth(auth) || has_openai_api_key(auth)
    })
}

pub(crate) fn capture_live_chatgpt_config(codex_dir: &Path) -> Result<bool> {
    capture_live_official_auth(codex_dir, is_chatgpt_auth)
}

fn capture_live_official_auth(
    codex_dir: &Path,
    is_trusted: impl FnOnce(&Value) -> bool,
) -> Result<bool> {
    if !live_config_is_official(codex_dir)? {
        return Ok(false);
    }
    let Some(auth) = read_auth_value(&auth_path(codex_dir))? else {
        return Ok(false);
    };
    if !is_trusted(&auth) {
        return Ok(false);
    }
    save_official_config_snapshot(codex_dir, official_model(codex_dir)?, &auth)?;
    Ok(true)
}

fn load_snapshot(codex_dir: &Path) -> Result<SnapshotState> {
    let path = official_snapshot_path(codex_dir)?;
    if !path.is_file() {
        return Ok(SnapshotState::Missing);
    }
    let text = fs::read_to_string(&path).map_err(|error| io_err(&path, error))?;
    let snapshot: OfficialConfigSnapshot =
        serde_json::from_str(&text).map_err(|error| json_err(&path, error))?;
    if !matches!(snapshot.version, SNAPSHOT_VERSION | LEGACY_SNAPSHOT_VERSION)
        || snapshot.codex_dir != canonical_identity(codex_dir)
    {
        return Err(CodexxError::Config(format!(
            "官方配置快照与当前 CODEX_HOME 不匹配: {}",
            path.display()
        )));
    }
    let Some(auth) = snapshot.auth else {
        return Ok(SnapshotState::Reset);
    };
    if !auth.is_object() || !auth_value_has_material(&auth) {
        return Err(CodexxError::Config(format!(
            "官方配置快照不包含可用认证: {}",
            path.display()
        )));
    }
    // Version 1 could be populated automatically from a proxy API key. Its
    // API-key-only snapshots are ambiguous, so never restore or promote them.
    if snapshot.version == LEGACY_SNAPSHOT_VERSION && !is_chatgpt_auth(&auth) {
        return Ok(SnapshotState::Missing);
    }
    Ok(SnapshotState::Ready(OfficialConfigCandidate {
        auth,
        model: snapshot.model,
        source: "Codex-X 官方配置快照".to_string(),
    }))
}

fn backup_config_is_official(dir: &Path, meta: &BackupMeta) -> bool {
    if !meta.had_config {
        return true;
    }
    let path = dir.join("config.toml");
    let Ok(text) = fs::read_to_string(&path) else {
        return false;
    };
    let Ok(doc) = parse_toml_document(&path, &text) else {
        return false;
    };
    document_is_official(&doc)
}

fn backup_model(dir: &Path, meta: &BackupMeta) -> Option<String> {
    if !meta.had_config {
        return None;
    }
    let path = dir.join("config.toml");
    let text = fs::read_to_string(&path).ok()?;
    let doc = parse_toml_document(&path, &text).ok()?;
    string_value(&doc, "model")
}

fn latest_official_backup(codex_dir: &Path) -> Result<Option<OfficialConfigCandidate>> {
    let root = action_backup_root(codex_dir)?;
    if !root.is_dir() {
        return Ok(None);
    }
    let identity = canonical_identity(codex_dir);
    let mut candidates = Vec::new();
    for entry in fs::read_dir(&root).map_err(|error| io_err(&root, error))? {
        let entry = entry.map_err(|error| io_err(&root, error))?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let meta_path = dir.join("meta.json");
        let Ok(meta_text) = fs::read_to_string(&meta_path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<BackupMeta>(&meta_text) else {
            continue;
        };
        if !meta.had_auth
            || canonical_identity(Path::new(&meta.codex_dir)) != identity
            || !backup_config_is_official(&dir, &meta)
        {
            continue;
        }
        let Ok(Some(auth)) = read_auth_value(&dir.join("auth.json")) else {
            continue;
        };
        // Old Codex-X versions could mark config.toml as official while leaving
        // a proxy API key in auth.json. Historical auto-recovery therefore only
        // trusts unambiguous ChatGPT login backups. Official API keys remain
        // supported through an explicit Codex-X snapshot/save.
        if !is_chatgpt_auth(&auth) {
            continue;
        }
        candidates.push((meta.created_at.clone(), dir, meta, auth));
    }
    candidates.sort_by(|left, right| right.0.cmp(&left.0));
    Ok(candidates
        .into_iter()
        .next()
        .map(|(_, dir, meta, auth)| OfficialConfigCandidate {
            auth,
            model: backup_model(&dir, &meta),
            source: format!("Codex-X 历史备份 {}", meta.created_at),
        }))
}

fn live_chatgpt_candidate(codex_dir: &Path) -> Result<Option<OfficialConfigCandidate>> {
    let Some(auth) = read_auth_value(&auth_path(codex_dir))? else {
        return Ok(None);
    };
    if !is_chatgpt_auth(&auth) {
        return Ok(None);
    }
    let model = if live_config_is_official(codex_dir)? {
        official_model(codex_dir)?
    } else {
        None
    };
    Ok(Some(OfficialConfigCandidate {
        auth,
        model,
        source: "当前 ChatGPT 官方登录".to_string(),
    }))
}

pub(crate) fn official_config_candidate(
    codex_dir: &Path,
    include_history_after_reset: bool,
) -> Result<Option<OfficialConfigCandidate>> {
    match load_snapshot(codex_dir)? {
        SnapshotState::Ready(candidate) => return Ok(Some(candidate)),
        SnapshotState::Reset if !include_history_after_reset => return Ok(None),
        SnapshotState::Missing | SnapshotState::Reset => {}
    }

    if let Some(candidate) = live_chatgpt_candidate(codex_dir)? {
        return Ok(Some(candidate));
    }
    latest_official_backup(codex_dir)
}

pub(crate) fn official_auth_available(codex_dir: &Path) -> Result<bool> {
    match load_snapshot(codex_dir)? {
        SnapshotState::Ready(_) => return Ok(true),
        SnapshotState::Reset => return Ok(false),
        SnapshotState::Missing => {}
    }
    if let Some(auth) = read_auth_value(&auth_path(codex_dir))? {
        if is_chatgpt_auth(&auth) {
            return Ok(true);
        }
    }
    Ok(latest_official_backup(codex_dir)?.is_some())
}

pub(crate) fn get_official_config_draft_inner(
    config_dir: Option<String>,
) -> Result<Option<OfficialConfigDraft>> {
    let codex_dir = crate::resolve_codex_dir(config_dir)?;
    let Some(candidate) = official_config_candidate(&codex_dir, true)? else {
        return Ok(None);
    };
    let auth_json = serde_json::to_string_pretty(&candidate.auth)
        .map_err(|error| CodexxError::Config(format!("格式化官方配置快照失败: {error}")))?;
    Ok(Some(OfficialConfigDraft {
        auth_json,
        model: candidate.model,
        source: candidate.source,
    }))
}

#[cfg(test)]
pub(crate) fn official_snapshot_path_for_test(codex_dir: &Path) -> Result<PathBuf> {
    official_snapshot_path(codex_dir)
}
