use super::ccswitch::codex_section_from_table;
use super::official_auth::{
    auth_value_has_material, capture_live_official_config_before_provider_switch,
    document_is_official, live_config_is_official, mark_official_config_reset,
    official_config_candidate, official_snapshot_path, save_official_config_snapshot,
};
use super::{
    delete_provider_inner, experimental_bearer_token_from_doc, is_placeholder_provider,
    list_saved_providers_inner, list_saved_providers_on_connection,
    matching_saved_provider_ids_for_live, normalize_saved_provider, open_store,
    provider_template_from_document, reserved_codex_provider_id, rollback_provider_store_inner,
    save_provider_with_rollback_inner, SavedProvider,
};
use crate::backups::create_backup;
use crate::config_migration::migrate_legacy_prompt_config_locked;
use crate::error::{CodexxError, Result};
use crate::file_io::{ensure_directory, json_err, parse_toml_document, read_to_string_if_exists};
use crate::live_config::{
    acquire_live_config_lock, atomic_write_if_unchanged, ensure_file_snapshot_unchanged,
    read_file_snapshot, remove_file_if_unchanged, restore_file_snapshot_if_unchanged,
    text_from_snapshot,
};
use crate::state::{build_state_after_migration, ActionResult};
use crate::toml_utils::ensure_table;
use crate::{auth_path, config_path, resolve_codex_dir, string_value};
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use toml_edit::{value, DocumentMut, Item};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderInput {
    pub(crate) config_dir: Option<String>,
    #[serde(rename = "providerId")]
    pub(crate) provider_id: Option<String>,
    pub(crate) provider_name: String,
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) api_key: Option<String>,
    pub(crate) wire_api: Option<String>,
    pub(crate) requires_openai_auth: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderTomlInput {
    pub(crate) config_dir: Option<String>,
    pub(crate) config_text: String,
    pub(crate) api_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OfficialConfigInput {
    pub(crate) config_dir: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) auth_json: Option<String>,
}

enum OfficialAuthAction {
    Keep,
    Replace(Value),
    Remove,
}

fn json_bytes(path: &Path, value: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|error| json_err(path, error))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn live_auth_api_key(codex_dir: &Path) -> Result<Option<String>> {
    let path = auth_path(codex_dir);
    let Some(bytes) = read_file_snapshot(&path)? else {
        return Ok(None);
    };
    let auth: Value = serde_json::from_slice(&bytes).map_err(|error| json_err(&path, error))?;
    Ok(auth
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(ToString::to_string))
}

struct AppliedLiveFiles {
    config_path: PathBuf,
    auth_path: PathBuf,
    old_config: Option<Vec<u8>>,
    old_auth: Option<Vec<u8>>,
    new_config: Vec<u8>,
    new_auth: Option<Option<Vec<u8>>>,
}

impl AppliedLiveFiles {
    fn rollback(&self) -> Result<()> {
        ensure_file_snapshot_unchanged(&self.config_path, Some(self.new_config.as_slice()))?;
        if let Some(new_auth) = &self.new_auth {
            ensure_file_snapshot_unchanged(&self.auth_path, new_auth.as_deref())?;
        }

        let mut failures = Vec::new();
        if let Some(new_auth) = &self.new_auth {
            if let Err(error) = restore_file_snapshot_if_unchanged(
                &self.auth_path,
                new_auth.as_deref(),
                self.old_auth.as_deref(),
            ) {
                failures.push(format!("auth.json: {error}"));
            }
        }
        if let Err(error) = restore_file_snapshot_if_unchanged(
            &self.config_path,
            Some(self.new_config.as_slice()),
            self.old_config.as_deref(),
        ) {
            failures.push(format!("config.toml: {error}"));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(CodexxError::Config(failures.join("；")))
        }
    }
}

struct AppliedSnapshot {
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
}

impl AppliedSnapshot {
    fn rollback(&self) -> Result<()> {
        restore_file_snapshot_if_unchanged(
            &self.path,
            self.after.as_deref(),
            self.before.as_deref(),
        )
    }
}

fn update_official_snapshot<F>(codex_dir: &Path, update: F) -> Result<AppliedSnapshot>
where
    F: FnOnce() -> Result<()>,
{
    let path = official_snapshot_path(codex_dir)?;
    let before = read_file_snapshot(&path)?;
    update()?;
    let after = read_file_snapshot(&path)?;
    Ok(AppliedSnapshot {
        path,
        before,
        after,
    })
}

fn capture_live_official_snapshot(codex_dir: &Path) -> Result<Option<AppliedSnapshot>> {
    let path = official_snapshot_path(codex_dir)?;
    let before = read_file_snapshot(&path)?;
    if !capture_live_official_config_before_provider_switch(codex_dir)? {
        return Ok(None);
    }
    let after = read_file_snapshot(&path)?;
    Ok(Some(AppliedSnapshot {
        path,
        before,
        after,
    }))
}

fn rollback_after_failure<T>(
    error: CodexxError,
    live: Option<&AppliedLiveFiles>,
    snapshot: Option<&AppliedSnapshot>,
) -> Result<T> {
    let mut failures = Vec::new();
    if let Some(snapshot) = snapshot {
        if let Err(rollback_error) = snapshot.rollback() {
            failures.push(format!("官方快照: {rollback_error}"));
        }
    }
    if let Some(live) = live {
        if let Err(rollback_error) = live.rollback() {
            failures.push(format!("live 配置: {rollback_error}"));
        }
    }
    if failures.is_empty() {
        Err(error)
    } else {
        Err(CodexxError::Config(format!(
            "{error}；回滚失败：{}",
            failures.join("；")
        )))
    }
}

fn write_live_files(
    codex_dir: &Path,
    old_config: Option<Vec<u8>>,
    old_auth: Option<Vec<u8>>,
    config_text: &str,
    auth_action: &OfficialAuthAction,
) -> Result<AppliedLiveFiles> {
    let cfg = config_path(codex_dir);
    let auth = auth_path(codex_dir);
    let new_config = config_text.as_bytes().to_vec();
    let new_auth = match auth_action {
        OfficialAuthAction::Keep => None,
        OfficialAuthAction::Replace(value) => Some(Some(json_bytes(&auth, value)?)),
        OfficialAuthAction::Remove => Some(None),
    };
    let mut config_written = false;

    let write_result = (|| -> Result<()> {
        atomic_write_if_unchanged(&cfg, old_config.as_deref(), &new_config)?;
        config_written = true;
        match &new_auth {
            None => Ok(()),
            Some(Some(bytes)) => atomic_write_if_unchanged(&auth, old_auth.as_deref(), bytes),
            Some(None) => remove_file_if_unchanged(&auth, old_auth.as_deref()),
        }
    })();

    if let Err(error) = write_result {
        if !config_written {
            return Err(error);
        }
        let rollback = restore_file_snapshot_if_unchanged(
            &cfg,
            Some(new_config.as_slice()),
            old_config.as_deref(),
        );
        if rollback.is_ok() {
            return Err(error);
        }
        return Err(CodexxError::Config(format!(
            "写入 Codex live 配置失败：{error}；config.toml 回滚也失败：{}",
            rollback.unwrap_err()
        )));
    }
    Ok(AppliedLiveFiles {
        config_path: cfg,
        auth_path: auth,
        old_config,
        old_auth,
        new_config,
        new_auth,
    })
}

fn write_live_config(
    codex_dir: &Path,
    old_config: Option<Vec<u8>>,
    replacement: Vec<u8>,
) -> Result<AppliedLiveFiles> {
    let cfg = config_path(codex_dir);
    atomic_write_if_unchanged(&cfg, old_config.as_deref(), &replacement)?;
    Ok(AppliedLiveFiles {
        config_path: cfg,
        auth_path: auth_path(codex_dir),
        old_config,
        old_auth: None,
        new_config: replacement,
        new_auth: None,
    })
}

pub(crate) fn detected_live_custom_provider(codex_dir: &Path) -> Result<Option<SavedProvider>> {
    let cfg = config_path(codex_dir);
    let text = read_to_string_if_exists(&cfg)?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    let doc = parse_toml_document(&cfg, &text)?;
    let Some(provider_id) = string_value(&doc, "model_provider") else {
        return Ok(None);
    };
    if document_is_official(&doc)
        || (provider_id != "custom" && reserved_codex_provider_id(&provider_id))
    {
        return Ok(None);
    }
    let Some(model) = string_value(&doc, "model") else {
        return Ok(None);
    };
    let Some(provider_table) = doc
        .get("model_providers")
        .and_then(|item| item.as_table())
        .and_then(|providers| providers.get(provider_id.as_str()))
        .and_then(|item| item.as_table())
    else {
        return Ok(None);
    };
    let Some(section) = codex_section_from_table(&provider_id, provider_table, Some(model.clone()))
    else {
        return Ok(None);
    };

    // Older switchers stored a third-party key in auth.json. Read it only after
    // this document has been proven to be a third-party route; it is used for
    // matching/migration and is never promoted to an official auth snapshot.
    let api_key = match experimental_bearer_token_from_doc(&doc, Some(&provider_id)) {
        Some(api_key) => Some(api_key),
        None => live_auth_api_key(codex_dir)?,
    };
    let provider_name = section
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| provider_id.clone());
    let toml_config = provider_template_from_document(&doc, &provider_id, &model)?;

    Ok(Some(SavedProvider {
        id: provider_id,
        provider_name,
        base_url: section.base_url,
        model,
        api_key,
        toml_config: (!toml_config.is_empty()).then_some(toml_config),
        wire_api: section.wire_api,
        requires_openai_auth: section.requires_openai_auth,
    }))
}

fn set_top_level_defaults(doc: &mut DocumentMut) {
    if doc.get("model_reasoning_effort").is_none() {
        doc["model_reasoning_effort"] = value("high");
    }
    if doc.get("disable_response_storage").is_none() {
        doc["disable_response_storage"] = value(true);
    }
}

fn set_provider_bearer_token(doc: &mut DocumentMut, token: &str) {
    let token = token.trim();
    if token.is_empty() {
        return;
    }
    let provider_id = string_value(doc, "model_provider");
    if let Some(provider_id) = provider_id {
        if let Some(provider_table) = doc
            .get_mut("model_providers")
            .and_then(|item| item.as_table_mut())
            .and_then(|providers| providers.get_mut(provider_id.as_str()))
            .and_then(|item| item.as_table_mut())
        {
            provider_table["experimental_bearer_token"] = value(token);
            return;
        }
    }
    doc["experimental_bearer_token"] = value(token);
}

fn apply_official_config_locked(
    codex_dir: &Path,
    model: Option<&str>,
    clear_model_if_none: bool,
    auth_action: OfficialAuthAction,
    action: &str,
) -> Result<(Option<String>, AppliedLiveFiles)> {
    let cfg = config_path(codex_dir);
    let auth = auth_path(codex_dir);
    let old_config = read_file_snapshot(&cfg)?;
    let old_auth = read_file_snapshot(&auth)?;
    let backup_id = create_backup(codex_dir, action)?;

    let text = text_from_snapshot(&cfg, old_config.as_deref())?;
    let mut doc = parse_toml_document(&cfg, &text)?;

    // Keep official and third-party conversations in the same stable bucket.
    // Omitting base_url preserves the real OpenAI backend and auth.json login.
    doc["model_provider"] = value("custom");
    doc.as_table_mut().remove("experimental_bearer_token");
    let providers = ensure_table(doc.as_table_mut(), "model_providers")?;
    providers.remove("custom");
    let official = ensure_table(providers, "custom")?;
    official["name"] = value("OpenAI");
    official["requires_openai_auth"] = value(true);
    official["supports_websockets"] = value(true);
    official["wire_api"] = value("responses");

    if let Some(model) = model.filter(|model| !model.trim().is_empty()) {
        doc["model"] = value(model);
    } else if clear_model_if_none {
        doc.as_table_mut().remove("model");
    }

    let applied = write_live_files(
        codex_dir,
        old_config,
        old_auth,
        &doc.to_string(),
        &auth_action,
    )?;
    Ok((backup_id, applied))
}

fn finish_live_action(
    codex_dir: &Path,
    message: String,
    backup_id: Option<String>,
    live: &AppliedLiveFiles,
    snapshot: Option<&AppliedSnapshot>,
) -> Result<ActionResult> {
    match build_state_after_migration(codex_dir.to_path_buf()) {
        Ok(state) => Ok(ActionResult {
            ok: true,
            message,
            backup_id,
            state,
        }),
        Err(error) => rollback_after_failure(error, Some(live), snapshot),
    }
}

pub(crate) fn switch_official_provider_with_pre_persist<F>(
    config_dir: Option<String>,
    pre_persist: F,
) -> Result<ActionResult>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let codex_dir = resolve_codex_dir(config_dir)?;
    ensure_directory(&codex_dir)?;
    let _live_lock = acquire_live_config_lock(&codex_dir)?;
    migrate_legacy_prompt_config_locked(&codex_dir)?;
    let was_official = live_config_is_official(&codex_dir)?;
    pre_persist(&codex_dir)?;
    let candidate = official_config_candidate(&codex_dir, false)?;
    let model = candidate
        .as_ref()
        .and_then(|candidate| candidate.model.clone());
    let auth_action = candidate
        .as_ref()
        .map(|candidate| OfficialAuthAction::Replace(candidate.auth.clone()))
        .unwrap_or(if was_official {
            // A user may legitimately use the built-in OpenAI provider with an
            // API key. It is safe to keep only while the live route is already
            // official; an API key seen under a proxy route remains ambiguous.
            OfficialAuthAction::Keep
        } else {
            OfficialAuthAction::Remove
        });
    let source = candidate.as_ref().map(|candidate| candidate.source.clone());
    let message = source
        .as_deref()
        .map(|source| format!("已切换到 OpenAI Official，并恢复：{source}"))
        .unwrap_or_else(|| "已切换到 OpenAI Official，请在 Codex 中完成登录".to_string());
    let (backup_id, live) = apply_official_config_locked(
        &codex_dir,
        model.as_deref(),
        !was_official && model.is_none(),
        auth_action,
        "switch-official",
    )?;
    let snapshot = if let Some(candidate) = candidate {
        match update_official_snapshot(&codex_dir, || {
            save_official_config_snapshot(&codex_dir, model, &candidate.auth)
        }) {
            Ok(snapshot) => Some(snapshot),
            Err(error) => return rollback_after_failure(error, Some(&live), None),
        }
    } else {
        None
    };
    finish_live_action(&codex_dir, message, backup_id, &live, snapshot.as_ref())
}

pub(crate) fn switch_official_provider_inner(config_dir: Option<String>) -> Result<ActionResult> {
    switch_official_provider_with_pre_persist(config_dir, |_| Ok(()))
}

pub(crate) fn save_official_config_inner(
    config_dir: Option<String>,
    model: Option<String>,
    auth_json: Option<String>,
) -> Result<ActionResult> {
    let codex_dir = resolve_codex_dir(config_dir)?;
    ensure_directory(&codex_dir)?;
    let _live_lock = acquire_live_config_lock(&codex_dir)?;
    migrate_legacy_prompt_config_locked(&codex_dir)?;
    let auth = auth_path(&codex_dir);
    let model = model
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let parsed_auth = if let Some(auth_json) = auth_json
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        let parsed: Value = serde_json::from_str(&auth_json).map_err(|e| json_err(&auth, e))?;
        if !parsed.is_object() || !auth_value_has_material(&parsed) {
            return Err(CodexxError::Config(
                "官方 auth.json 必须是包含有效认证信息的 JSON object".to_string(),
            ));
        }
        parsed
    } else {
        official_config_candidate(&codex_dir, true)?
            .map(|candidate| candidate.auth)
            .ok_or_else(|| {
                CodexxError::Config("没有可保存的官方认证，请先完成官方登录".to_string())
            })?
    };

    if live_config_is_official(&codex_dir)? {
        let (backup_id, live) = apply_official_config_locked(
            &codex_dir,
            model.as_deref(),
            false,
            OfficialAuthAction::Replace(parsed_auth.clone()),
            "save-official",
        )?;
        let snapshot = match update_official_snapshot(&codex_dir, || {
            save_official_config_snapshot(&codex_dir, model, &parsed_auth)
        }) {
            Ok(snapshot) => snapshot,
            Err(error) => return rollback_after_failure(error, Some(&live), None),
        };
        return finish_live_action(
            &codex_dir,
            "已保存并更新当前 OpenAI Official 配置".to_string(),
            backup_id,
            &live,
            Some(&snapshot),
        );
    }

    let backup_id = create_backup(&codex_dir, "save-official-snapshot")?;
    let snapshot = update_official_snapshot(&codex_dir, || {
        save_official_config_snapshot(&codex_dir, model, &parsed_auth)
    })?;
    match build_state_after_migration(codex_dir.clone()) {
        Ok(state) => Ok(ActionResult {
            ok: true,
            message: "已保存 OpenAI Official 独立配置，当前中转配置未被改动".to_string(),
            backup_id,
            state,
        }),
        Err(error) => rollback_after_failure(error, None, Some(&snapshot)),
    }
}

pub(crate) fn restore_official_provider_inner(config_dir: Option<String>) -> Result<ActionResult> {
    let codex_dir = resolve_codex_dir(config_dir)?;
    ensure_directory(&codex_dir)?;
    let _live_lock = acquire_live_config_lock(&codex_dir)?;
    migrate_legacy_prompt_config_locked(&codex_dir)?;
    let candidate = official_config_candidate(&codex_dir, true)?.ok_or_else(|| {
        CodexxError::Config(
            "未找到可信的官方认证快照或官方模式历史备份，请新建官方配置后重新登录".to_string(),
        )
    })?;
    let model = candidate.model.clone();
    let message = format!(
        "已还原 OpenAI Official 独立配置，当前供应商未切换。认证来源：{}",
        candidate.source
    );
    let snapshot = update_official_snapshot(&codex_dir, || {
        save_official_config_snapshot(&codex_dir, model, &candidate.auth)
    })?;
    match build_state_after_migration(codex_dir.clone()) {
        Ok(state) => Ok(ActionResult {
            ok: true,
            message,
            backup_id: None,
            state,
        }),
        Err(error) => rollback_after_failure(error, None, Some(&snapshot)),
    }
}

pub(crate) fn reset_official_provider_inner(
    config_dir: Option<String>,
    model: Option<String>,
) -> Result<ActionResult> {
    let codex_dir = resolve_codex_dir(config_dir)?;
    ensure_directory(&codex_dir)?;
    let _live_lock = acquire_live_config_lock(&codex_dir)?;
    migrate_legacy_prompt_config_locked(&codex_dir)?;
    let model = model
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let (backup_id, live) = apply_official_config_locked(
        &codex_dir,
        model.as_deref(),
        model.is_none(),
        OfficialAuthAction::Remove,
        "reset-official",
    )?;
    let snapshot = match update_official_snapshot(&codex_dir, || {
        mark_official_config_reset(&codex_dir, model)
    }) {
        Ok(snapshot) => snapshot,
        Err(error) => return rollback_after_failure(error, Some(&live), None),
    };
    finish_live_action(
        &codex_dir,
        "已新建 OpenAI Official 配置，请在 Codex 中重新登录".to_string(),
        backup_id,
        &live,
        Some(&snapshot),
    )
}

fn merge_provider_toml_into_live(
    cfg: &Path,
    current_text: &str,
    provider_text: &str,
    explicit_api_key: Option<String>,
) -> Result<DocumentMut> {
    let source = parse_toml_document(cfg, provider_text)?;
    let model = string_value(&source, "model")
        .ok_or_else(|| CodexxError::Config("config.toml 必须包含 model".to_string()))?;
    let source_provider_id = string_value(&source, "model_provider")
        .ok_or_else(|| CodexxError::Config("config.toml 必须包含 model_provider".to_string()))?;
    let mut source_provider = source
        .get("model_providers")
        .and_then(|item| item.as_table())
        .and_then(|providers| providers.get(source_provider_id.as_str()))
        .and_then(|item| item.as_table())
        .cloned()
        .ok_or_else(|| {
            CodexxError::Config(format!(
                "config.toml 缺少 [model_providers.{source_provider_id}]"
            ))
        })?;
    let source_name = source_provider
        .get("name")
        .and_then(|item| item.as_str())
        .unwrap_or_default();
    if source_provider
        .get("base_url")
        .and_then(|item| item.as_str())
        .is_none_or(|value| value.trim().is_empty())
    {
        return Err(CodexxError::Config(
            "供应商配置必须包含非空 base_url".to_string(),
        ));
    }
    if is_placeholder_provider(
        source_name,
        source_provider
            .get("base_url")
            .and_then(|item| item.as_str())
            .unwrap_or_default(),
    ) {
        return Err(CodexxError::Config(
            "供应商名称和 base_url 不能使用示例占位值，请填写实际配置".to_string(),
        ));
    }

    let api_key = explicit_api_key
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| experimental_bearer_token_from_doc(&source, Some(source_provider_id.as_str())));
    source_provider.remove("experimental_bearer_token");
    if let Some(api_key) = api_key {
        source_provider["experimental_bearer_token"] = value(api_key);
    }

    let mut live = parse_toml_document(cfg, current_text)?;
    live["model_provider"] = value("custom");
    live["model"] = value(model);
    live.as_table_mut().remove("experimental_bearer_token");
    set_top_level_defaults(&mut live);
    let providers = ensure_table(live.as_table_mut(), "model_providers")?;
    providers.remove("custom");
    providers.insert("custom", Item::Table(source_provider));
    Ok(live)
}

fn save_provider_toml_config_locked<F>(
    codex_dir: &Path,
    input: ProviderTomlInput,
    old_config: Option<Vec<u8>>,
    pre_persist: F,
) -> Result<ActionResult>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let cfg = config_path(codex_dir);
    let snapshot = capture_live_official_snapshot(codex_dir)?;
    let prepared = (|| -> Result<(Option<String>, Vec<u8>)> {
        pre_persist(codex_dir)?;
        let backup_id = create_backup(codex_dir, "save-provider-toml")?;
        let current_text = text_from_snapshot(&cfg, old_config.as_deref())?;
        let doc = merge_provider_toml_into_live(
            &cfg,
            &current_text,
            input.config_text.trim_end(),
            input.api_key,
        )?;
        let replacement = doc.to_string().trim_end().to_string() + "\n";
        Ok((backup_id, replacement.into_bytes()))
    })();
    let (backup_id, replacement) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return rollback_after_failure(error, None, snapshot.as_ref()),
    };
    let live = match write_live_config(codex_dir, old_config, replacement) {
        Ok(live) => live,
        Err(error) => return rollback_after_failure(error, None, snapshot.as_ref()),
    };
    finish_live_action(
        codex_dir,
        "已保存供应商 TOML 配置".to_string(),
        backup_id,
        &live,
        snapshot.as_ref(),
    )
}

pub(crate) fn save_provider_toml_config_with_pre_persist<F>(
    input: ProviderTomlInput,
    pre_persist: F,
) -> Result<ActionResult>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let codex_dir = resolve_codex_dir(input.config_dir.clone())?;
    ensure_directory(&codex_dir)?;
    let _live_lock = acquire_live_config_lock(&codex_dir)?;
    migrate_legacy_prompt_config_locked(&codex_dir)?;
    let old_config = read_file_snapshot(&config_path(&codex_dir))?;
    save_provider_toml_config_locked(&codex_dir, input, old_config, pre_persist)
}

pub(crate) fn save_provider_toml_config_inner(input: ProviderTomlInput) -> Result<ActionResult> {
    save_provider_toml_config_with_pre_persist(input, |_| Ok(()))
}

fn switch_provider_locked<F>(
    codex_dir: &Path,
    input: ProviderInput,
    old_config: Option<Vec<u8>>,
    pre_persist: F,
) -> Result<ActionResult>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let provider_name = input.provider_name.trim();
    if input
        .provider_id
        .as_deref()
        .is_some_and(|provider_id| provider_id.trim().is_empty())
    {
        return Err(CodexxError::Config("供应商 ID 不能为空".to_string()));
    }
    // CC Switch uses a stable live key for all third-party providers. The
    // logical saved id remains in Codex-X storage and is matched by backend.
    let live_provider_key = "custom";
    let base_url = input.base_url.trim().trim_end_matches('/');
    let model = input.model.trim();
    if provider_name.is_empty() {
        return Err(CodexxError::Config("供应商名称不能为空".to_string()));
    }
    if base_url.is_empty() {
        return Err(CodexxError::Config("base_url 不能为空".to_string()));
    }
    if model.is_empty() {
        return Err(CodexxError::Config("model 不能为空".to_string()));
    }
    if is_placeholder_provider(provider_name, base_url) {
        return Err(CodexxError::Config(
            "供应商名称和 base_url 不能使用示例占位值，请填写实际配置".to_string(),
        ));
    }

    let cfg = config_path(codex_dir);
    let snapshot = capture_live_official_snapshot(codex_dir)?;
    let prepared = (|| -> Result<(Option<String>, Vec<u8>)> {
        pre_persist(codex_dir)?;
        let backup_id = create_backup(codex_dir, "switch-provider")?;
        let text = text_from_snapshot(&cfg, old_config.as_deref())?;
        let mut doc = parse_toml_document(&cfg, &text)?;
        doc["model_provider"] = value(live_provider_key);
        doc["model"] = value(model);
        doc.as_table_mut().remove("experimental_bearer_token");
        set_top_level_defaults(&mut doc);

        let root = doc.as_table_mut();
        let providers = ensure_table(root, "model_providers")?;
        providers.remove(live_provider_key);
        let provider_table = ensure_table(providers, live_provider_key)?;
        provider_table["name"] = value(provider_name);
        provider_table["base_url"] = value(base_url);
        provider_table["wire_api"] =
            value(input.wire_api.unwrap_or_else(|| "responses".to_string()));
        provider_table["requires_openai_auth"] = value(input.requires_openai_auth.unwrap_or(true));

        let api_key = input
            .api_key
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(api_key) = api_key.as_deref() {
            set_provider_bearer_token(&mut doc, api_key);
        }
        Ok((backup_id, doc.to_string().into_bytes()))
    })();
    let (backup_id, replacement) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return rollback_after_failure(error, None, snapshot.as_ref()),
    };
    let live = match write_live_config(codex_dir, old_config, replacement) {
        Ok(live) => live,
        Err(error) => return rollback_after_failure(error, None, snapshot.as_ref()),
    };
    finish_live_action(
        codex_dir,
        format!("已切换到 {provider_name} / {model}"),
        backup_id,
        &live,
        snapshot.as_ref(),
    )
}

pub(crate) fn switch_provider_with_pre_persist<F>(
    input: ProviderInput,
    pre_persist: F,
) -> Result<ActionResult>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let codex_dir = resolve_codex_dir(input.config_dir.clone())?;
    ensure_directory(&codex_dir)?;
    let _live_lock = acquire_live_config_lock(&codex_dir)?;
    migrate_legacy_prompt_config_locked(&codex_dir)?;
    let old_config = read_file_snapshot(&config_path(&codex_dir))?;
    switch_provider_locked(&codex_dir, input, old_config, pre_persist)
}

pub(crate) fn switch_provider_inner(input: ProviderInput) -> Result<ActionResult> {
    switch_provider_with_pre_persist(input, |_| Ok(()))
}

fn save_active_provider_with_apply<F>(
    provider: SavedProvider,
    config_dir: Option<String>,
    apply: F,
) -> Result<ActionResult>
where
    F: FnOnce(&SavedProvider, &Path, Option<Vec<u8>>) -> Result<ActionResult>,
{
    let provider = normalize_saved_provider(provider)?;
    let codex_dir = resolve_codex_dir(config_dir)?;
    ensure_directory(&codex_dir)?;
    let _live_lock = acquire_live_config_lock(&codex_dir)?;
    migrate_legacy_prompt_config_locked(&codex_dir)?;
    let active_config = read_file_snapshot(&config_path(&codex_dir))?;
    let conn = open_store()?;
    let saved_before = list_saved_providers_on_connection(&conn)?;
    let live = detected_live_custom_provider(&codex_dir)?.ok_or_else(|| {
        CodexxError::Config("当前不是可编辑的第三方供应商，未修改保存记录".to_string())
    })?;
    let matches = matching_saved_provider_ids_for_live(&live, &saved_before);
    match matches.as_slice() {
        [] => {
            if saved_before
                .iter()
                .any(|candidate| candidate.id == provider.id)
            {
                return Err(CodexxError::Config(format!(
                    "供应商 ID {} 已被另一条配置使用，请更换名称后再保存",
                    provider.id
                )));
            }
        }
        [active_id] if active_id == &provider.id => {}
        [active_id] => {
            return Err(CodexxError::Config(format!(
                "当前启用的是 {active_id}，不能把 {} 作为活动配置保存",
                provider.id
            )));
        }
        _ => {
            return Err(CodexxError::Config(
                "当前 live 供应商匹配到多条保存记录，请先清理重复配置".to_string(),
            ));
        }
    }

    let (saved, rollback) = save_provider_with_rollback_inner(provider)?;
    match apply(&saved, &codex_dir, active_config) {
        Ok(mut result) => {
            result.message = "供应商配置已保存并热更新".to_string();
            Ok(result)
        }
        Err(error) => match rollback_provider_store_inner(rollback) {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(CodexxError::Database(format!(
                "热更新供应商失败: {error}；数据库回滚也失败: {rollback_error}"
            ))),
        },
    }
}

pub(crate) fn save_active_provider_inner(
    provider: SavedProvider,
    config_dir: Option<String>,
) -> Result<ActionResult> {
    save_active_provider_with_apply(provider, config_dir, |saved, codex_dir, active_config| {
        if let Some(config_text) = saved.toml_config.clone() {
            save_provider_toml_config_locked(
                codex_dir,
                ProviderTomlInput {
                    config_dir: None,
                    config_text,
                    api_key: saved.api_key.clone(),
                },
                active_config,
                |_| Ok(()),
            )
        } else {
            switch_provider_locked(
                codex_dir,
                ProviderInput {
                    config_dir: None,
                    provider_id: Some(saved.id.clone()),
                    provider_name: saved.provider_name.clone(),
                    base_url: saved.base_url.clone(),
                    model: saved.model.clone(),
                    api_key: saved.api_key.clone(),
                    wire_api: Some(saved.wire_api.clone()),
                    requires_openai_auth: Some(saved.requires_openai_auth),
                },
                active_config,
                |_| Ok(()),
            )
        }
    })
}

pub(crate) fn delete_saved_provider_inner(id: &str, config_dir: Option<String>) -> Result<()> {
    let id = id.trim();
    if id.is_empty() {
        return Err(CodexxError::Config("供应商 ID 不能为空".to_string()));
    }
    let codex_dir = resolve_codex_dir(config_dir)?;
    let providers = list_saved_providers_inner()?;
    if let Some(live) = detected_live_custom_provider(&codex_dir)? {
        let active_ids = matching_saved_provider_ids_for_live(&live, &providers);
        if live.id == id || active_ids.iter().any(|active_id| active_id == id) {
            return Err(CodexxError::Config(
                "不能直接删除当前启用的供应商，请先切换到官方配置或其他供应商".to_string(),
            ));
        }
    }
    delete_provider_inner(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_io::{write_json, write_text};
    use crate::providers::delete_provider_inner;
    use crate::providers::{list_saved_providers_inner, save_provider_inner};
    use serde_json::json;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn active_provider_fixture(
        tag: u64,
        id: &str,
        name: &str,
        model: &str,
        key: &str,
    ) -> SavedProvider {
        let base_url = format!("https://active-{tag}.example.com/v1");
        SavedProvider {
            id: id.to_string(),
            provider_name: name.to_string(),
            base_url: base_url.clone(),
            model: model.to_string(),
            api_key: Some(key.to_string()),
            toml_config: Some(format!(
                r#"model_provider = "custom"
model = "{model}"

[model_providers.custom]
name = "{name}"
base_url = "{base_url}"
wire_api = "responses"
requires_openai_auth = false
"#
            )),
            wire_api: "responses".to_string(),
            requires_openai_auth: false,
        }
    }

    fn active_provider_test_dir(label: &str, tag: u64) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "codex-x-active-provider-{label}-{}-{tag}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create active provider test directory");
        path
    }

    #[test]
    fn direct_switch_rejects_placeholder_provider_values() {
        let codex_dir = active_provider_test_dir("placeholder", 30_000);
        let error = switch_provider_inner(ProviderInput {
            config_dir: Some(codex_dir.display().to_string()),
            provider_id: Some("placeholder".to_string()),
            provider_name: "your-provider".to_string(),
            base_url: "https://example.com/v1".to_string(),
            model: "gpt-5.5".to_string(),
            api_key: None,
            wire_api: Some("responses".to_string()),
            requires_openai_auth: Some(false),
        })
        .expect_err("placeholder provider must be rejected");
        assert!(error.to_string().contains("示例占位值"));
        assert!(!config_path(&codex_dir).exists());
        fs::remove_dir_all(codex_dir).expect("remove placeholder test directory");
    }

    fn write_active_provider_files(codex_dir: &Path, provider: &SavedProvider) -> Value {
        let token = provider.api_key.as_deref().expect("provider key");
        let mut doc = provider
            .toml_config
            .as_deref()
            .expect("provider TOML")
            .parse::<DocumentMut>()
            .expect("parse live config");
        doc["approval_policy"] = value("never");
        let mcp_servers =
            ensure_table(doc.as_table_mut(), "mcp_servers").expect("create MCP provider table");
        let docs = ensure_table(mcp_servers, "docs").expect("create MCP docs table");
        docs["command"] = value("docs-server");
        set_provider_bearer_token(&mut doc, token);
        write_text(&config_path(codex_dir), &doc.to_string()).expect("write live config");
        let auth = json!({
            "auth_mode": "chatgpt",
            "tokens": {"access_token": format!("official-{token}")}
        });
        write_json(&auth_path(codex_dir), &auth).expect("write live auth");
        auth
    }

    fn saved_provider(id: &str) -> SavedProvider {
        list_saved_providers_inner()
            .expect("list saved providers")
            .into_iter()
            .find(|provider| provider.id == id)
            .expect("saved provider")
    }

    #[test]
    fn active_provider_save_updates_one_record_and_hot_applies() {
        let _db_guard = crate::app_db::test_db_guard();
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let tag = COUNTER.fetch_add(1, Ordering::Relaxed) + 10_000;
        let id = format!("active-save-{tag}");
        let codex_dir = active_provider_test_dir("success", tag);
        let original = active_provider_fixture(tag, &id, "Before", "model-before", "sk-before");
        save_provider_inner(original.clone()).expect("save original provider");
        let official_auth = write_active_provider_files(&codex_dir, &original);

        let mut updated = original.clone();
        updated.provider_name = "After".to_string();
        updated.model = "model-after".to_string();
        updated.api_key = Some("sk-after".to_string());
        let result = save_active_provider_inner(updated, Some(codex_dir.display().to_string()))
            .expect("save active provider");

        assert!(!result.state.is_official_provider);
        let state = serde_json::to_value(&result.state).expect("serialize updated state");
        assert_eq!(state["activeSavedProviderId"].as_str(), Some(id.as_str()),);
        let live = fs::read_to_string(config_path(&codex_dir)).expect("read updated live config");
        let doc = live
            .parse::<DocumentMut>()
            .expect("parse updated live config");
        assert_eq!(doc["model"].as_str(), Some("model-after"));
        assert_eq!(doc["approval_policy"].as_str(), Some("never"));
        assert_eq!(
            doc["mcp_servers"]["docs"]["command"].as_str(),
            Some("docs-server")
        );
        assert_eq!(
            doc["model_providers"]["custom"]["experimental_bearer_token"].as_str(),
            Some("sk-after")
        );
        assert_eq!(saved_provider(&id).provider_name, "After");
        let auth_after: Value = serde_json::from_str(
            &fs::read_to_string(auth_path(&codex_dir)).expect("read official auth"),
        )
        .expect("parse official auth");
        assert_eq!(auth_after, official_auth);
        let delete_error = delete_saved_provider_inner(&id, Some(codex_dir.display().to_string()))
            .expect_err("active provider deletion must be blocked");
        assert!(delete_error.to_string().contains("不能直接删除当前启用"));

        delete_provider_inner(&id).expect("delete test provider");
        fs::remove_dir_all(codex_dir).expect("remove active provider test directory");
    }

    #[test]
    fn active_provider_save_rolls_back_record_when_apply_fails_before_writing() {
        let _db_guard = crate::app_db::test_db_guard();
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let tag = COUNTER.fetch_add(1, Ordering::Relaxed) + 20_000;
        let id = format!("active-rollback-{tag}");
        let codex_dir = active_provider_test_dir("rollback", tag);
        let original = active_provider_fixture(tag, &id, "Before", "model-before", "sk-before");
        save_provider_inner(original.clone()).expect("save original provider");
        write_active_provider_files(&codex_dir, &original);
        let config_before = fs::read(config_path(&codex_dir)).expect("snapshot config");
        let auth_before = fs::read(auth_path(&codex_dir)).expect("snapshot auth");

        let mut updated = original.clone();
        updated.provider_name = "Must Roll Back".to_string();
        updated.model = "model-after".to_string();
        updated.api_key = Some("sk-after".to_string());
        let error = save_active_provider_with_apply(
            updated,
            Some(codex_dir.display().to_string()),
            |_, _, _| Err(CodexxError::Config("injected apply failure".to_string())),
        )
        .expect_err("injected apply failure must roll back");

        assert!(error.to_string().contains("injected apply failure"));
        assert_eq!(saved_provider(&id).provider_name, "Before");
        assert_eq!(
            fs::read(config_path(&codex_dir)).expect("read rolled back config"),
            config_before
        );
        assert_eq!(
            fs::read(auth_path(&codex_dir)).expect("read rolled back auth"),
            auth_before
        );

        delete_provider_inner(&id).expect("delete test provider");
        fs::remove_dir_all(codex_dir).expect("remove active provider test directory");
    }

    #[test]
    fn provider_switch_rolls_back_live_config_when_final_state_build_fails() {
        let tag = 30_004;
        let codex_dir = active_provider_test_dir("post-write-state-failure", tag);
        let original =
            active_provider_fixture(tag, "custom", "Before", "model-before", "sk-before");
        write_active_provider_files(&codex_dir, &original);
        fs::write(auth_path(&codex_dir), b"{malformed-auth").expect("write malformed live auth");
        let config_before = fs::read(config_path(&codex_dir)).expect("snapshot live config");

        let error = switch_provider_inner(ProviderInput {
            config_dir: Some(codex_dir.display().to_string()),
            provider_id: Some("next".to_string()),
            provider_name: "Next".to_string(),
            base_url: "https://next.example.com/v1".to_string(),
            model: "model-next".to_string(),
            api_key: Some("sk-next".to_string()),
            wire_api: Some("responses".to_string()),
            requires_openai_auth: Some(false),
        })
        .expect_err("malformed auth must fail final state construction");

        assert!(error.to_string().contains("auth.json"));
        assert_eq!(
            fs::read(config_path(&codex_dir)).expect("read rolled back live config"),
            config_before
        );
        assert_eq!(
            fs::read(auth_path(&codex_dir)).expect("read preserved malformed auth"),
            b"{malformed-auth"
        );

        fs::remove_dir_all(codex_dir).expect("remove state failure test directory");
    }

    #[test]
    fn active_provider_edit_rolls_back_database_and_live_config_after_state_failure() {
        let _db_guard = crate::app_db::test_db_guard();
        let tag = 30_005;
        let id = format!("active-post-write-rollback-{tag}");
        let codex_dir = active_provider_test_dir("active-post-write-state-failure", tag);
        let original = active_provider_fixture(tag, &id, "Before", "model-before", "sk-before");
        save_provider_inner(original.clone()).expect("save original provider");
        write_active_provider_files(&codex_dir, &original);
        fs::write(auth_path(&codex_dir), b"{malformed-auth").expect("write malformed live auth");
        let config_before = fs::read(config_path(&codex_dir)).expect("snapshot live config");

        let mut updated = original.clone();
        updated.provider_name = "After".to_string();
        updated.model = "model-after".to_string();
        updated.api_key = Some("sk-after".to_string());
        let error = save_active_provider_inner(updated, Some(codex_dir.display().to_string()))
            .expect_err("malformed auth must fail final state construction");

        assert!(error.to_string().contains("auth.json"));
        let restored = saved_provider(&id);
        assert_eq!(restored.provider_name, original.provider_name);
        assert_eq!(restored.model, original.model);
        assert_eq!(restored.api_key, original.api_key);
        assert_eq!(
            fs::read(config_path(&codex_dir)).expect("read rolled back live config"),
            config_before
        );
        assert_eq!(
            fs::read(auth_path(&codex_dir)).expect("read preserved malformed auth"),
            b"{malformed-auth"
        );

        delete_provider_inner(&id).expect("delete test provider");
        fs::remove_dir_all(codex_dir).expect("remove state failure test directory");
    }

    #[test]
    fn live_config_lock_rejects_a_second_writer() {
        let codex_dir = active_provider_test_dir("lock", 30_001);
        ensure_directory(&codex_dir).expect("create test Codex directory");

        let first = acquire_live_config_lock(&codex_dir).expect("acquire first live lock");
        let error = acquire_live_config_lock(&codex_dir)
            .err()
            .expect("second live lock must fail");
        assert!(error.to_string().contains("另一个 Codex-X"));
        drop(first);
        acquire_live_config_lock(&codex_dir).expect("lock is released on drop");

        fs::remove_dir_all(codex_dir).expect("remove live lock test directory");
    }

    #[test]
    fn provider_switch_rejects_a_stale_read_and_preserves_external_toml() {
        let codex_dir = active_provider_test_dir("stale-config", 30_002);
        ensure_directory(&codex_dir).expect("create test Codex directory");
        let cfg = config_path(&codex_dir);
        write_text(
            &cfg,
            "model_provider = \"custom\"\nmodel = \"before\"\n\n[model_providers.custom]\nname = \"Before\"\nbase_url = \"https://before.example/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = false\n",
        )
        .expect("write initial config");
        let external = "model = \"external-change\"\napproval_policy = \"never\"\n";

        let error = switch_provider_with_pre_persist(
            ProviderInput {
                config_dir: Some(codex_dir.display().to_string()),
                provider_id: Some("next".to_string()),
                provider_name: "Next".to_string(),
                base_url: "https://next.example/v1".to_string(),
                model: "next-model".to_string(),
                api_key: Some("sk-next".to_string()),
                wire_api: Some("responses".to_string()),
                requires_openai_auth: Some(false),
            },
            |dir| write_text(&config_path(dir), external),
        )
        .expect_err("stale config write must be rejected");

        assert!(error.to_string().contains("已被其他程序修改"));
        assert_eq!(
            fs::read_to_string(&cfg).expect("read externally changed config"),
            external
        );
        fs::remove_dir_all(codex_dir).expect("remove stale config test directory");
    }

    #[test]
    fn active_provider_edit_rejects_live_change_after_detection_and_restores_record() {
        let _db_guard = crate::app_db::test_db_guard();
        let tag = 30_006;
        let id = format!("active-stale-edit-{tag}");
        let codex_dir = active_provider_test_dir("active-stale-edit", tag);
        let original = active_provider_fixture(tag, &id, "Before", "model-before", "sk-before");
        save_provider_inner(original.clone()).expect("save original provider");
        write_active_provider_files(&codex_dir, &original);
        let external = r#"model_provider = "custom"
model = "external-model"

[model_providers.custom]
name = "External"
base_url = "https://external.example.com/v1"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "sk-external"
"#;
        let mut updated = original.clone();
        updated.provider_name = "After".to_string();
        updated.model = "model-after".to_string();
        updated.api_key = Some("sk-after".to_string());

        let error = save_active_provider_with_apply(
            updated,
            Some(codex_dir.display().to_string()),
            |saved, codex_dir, active_config| {
                write_text(&config_path(codex_dir), external)?;
                save_provider_toml_config_locked(
                    codex_dir,
                    ProviderTomlInput {
                        config_dir: None,
                        config_text: saved.toml_config.clone().expect("provider TOML"),
                        api_key: saved.api_key.clone(),
                    },
                    active_config,
                    |_| Ok(()),
                )
            },
        )
        .expect_err("live provider change must reject stale active edit");

        assert!(error.to_string().contains("已被其他程序修改"));
        assert_eq!(
            fs::read_to_string(config_path(&codex_dir)).expect("read external live config"),
            external
        );
        assert_eq!(saved_provider(&id).provider_name, original.provider_name);

        delete_provider_inner(&id).expect("delete test provider");
        fs::remove_dir_all(codex_dir).expect("remove stale active edit test directory");
    }

    #[test]
    fn detected_live_provider_can_be_adopted_saved_and_hot_applied() {
        let _db_guard = crate::app_db::test_db_guard();
        let tag = 30_003;
        let id = format!("detected-adopt-{tag}");
        let codex_dir = active_provider_test_dir("detected-adopt", tag);
        let live = active_provider_fixture(tag, "custom", "Detected", "before", "sk-before");
        write_active_provider_files(&codex_dir, &live);

        let adopted = active_provider_fixture(tag, &id, "Adopted", "after", "sk-after");
        let result = save_active_provider_inner(adopted, Some(codex_dir.display().to_string()))
            .expect("adopt detected live provider");
        assert_eq!(result.state.model.as_deref(), Some("after"));
        assert_eq!(saved_provider(&id).provider_name, "Adopted");

        delete_provider_inner(&id).expect("delete adopted provider");
        fs::remove_dir_all(codex_dir).expect("remove detected provider test directory");
    }
}
