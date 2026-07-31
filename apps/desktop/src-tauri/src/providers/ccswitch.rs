use super::{
    consolidate_legacy_provider_duplicates_on_connection, custom_provider_id,
    experimental_bearer_token_from_doc, list_saved_providers_on_connection,
    normalize_saved_provider, open_store, upsert_ccswitch_provider_on_connection,
    ProviderUpsertKind, SavedProvider,
};
use crate::ccswitch::{ccswitch_db_candidates, default_ccswitch_db_path};
use crate::error::{CodexxError, Result};
use crate::sqlite_utils::table_column_set;
use crate::string_value;
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use toml_edit::{DocumentMut, Table};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportResult {
    imported: usize,
    added: usize,
    updated: usize,
    merged: usize,
    skipped: usize,
    warnings: Vec<String>,
    providers: Vec<SavedProvider>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OfficialAuthCandidate {
    auth_json: String,
    model: Option<String>,
    source: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CcSwitchCodexRow {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) settings_config: String,
    pub(crate) category: Option<String>,
}

pub(crate) fn is_official_ccswitch_row(row: &CcSwitchCodexRow) -> bool {
    row.id.trim().eq_ignore_ascii_case("codex-official")
        || row
            .category
            .as_deref()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("official"))
}

pub(crate) fn read_ccswitch_codex_rows(conn: &Connection) -> Result<Vec<CcSwitchCodexRow>> {
    let provider_columns = table_column_set(conn, "providers")?;
    let category_column = if provider_columns.contains("category") {
        "category"
    } else {
        "NULL"
    };
    let provider_query = format!(
        "SELECT id, name, settings_config, {category_column} FROM providers
         WHERE app_type = 'codex' ORDER BY sort_index ASC, created_at ASC"
    );
    let mut stmt = conn
        .prepare(&provider_query)
        .map_err(|e| CodexxError::Database(e.to_string()))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(CcSwitchCodexRow {
                id: row.get::<_, String>(0)?,
                name: row.get::<_, String>(1)?,
                settings_config: row.get::<_, String>(2)?,
                category: row.get::<_, Option<String>>(3)?,
            })
        })
        .map_err(|e| CodexxError::Database(e.to_string()))?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.map_err(|e| CodexxError::Database(e.to_string()))?);
    }
    Ok(result)
}

#[derive(Debug, Clone)]
pub(crate) struct CcSwitchCodexSection {
    pub(crate) id: String,
    pub(crate) name: Option<String>,
    pub(crate) base_url: String,
    pub(crate) model: Option<String>,
    pub(crate) wire_api: String,
    pub(crate) requires_openai_auth: bool,
    pub(crate) experimental_bearer_token: Option<String>,
}

fn table_string(table: &Table, key: &str) -> Option<String> {
    table
        .get(key)
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn ccswitch_auth_api_key(settings: &Value) -> Option<String> {
    settings
        .get("auth")
        .and_then(|v| v.get("OPENAI_API_KEY"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

pub(super) fn codex_section_from_table(
    id: &str,
    table: &Table,
    model: Option<String>,
) -> Option<CcSwitchCodexSection> {
    let base_url = table_string(table, "base_url")?
        .trim_end_matches('/')
        .to_string();
    if base_url.is_empty() {
        return None;
    }
    Some(CcSwitchCodexSection {
        id: id.to_string(),
        name: table_string(table, "name"),
        base_url,
        model,
        wire_api: table_string(table, "wire_api").unwrap_or_else(|| "responses".to_string()),
        requires_openai_auth: table
            .get("requires_openai_auth")
            .and_then(|item| item.as_bool())
            .unwrap_or(false),
        experimental_bearer_token: table_string(table, "experimental_bearer_token"),
    })
}

pub(crate) fn codex_sections_from_config(config_text: &str) -> Vec<CcSwitchCodexSection> {
    let Ok(doc) = config_text.parse::<DocumentMut>() else {
        return Vec::new();
    };
    let model = string_value(&doc, "model");
    let Some(providers) = doc.get("model_providers").and_then(|item| item.as_table()) else {
        return Vec::new();
    };
    providers
        .iter()
        .filter_map(|(id, item)| {
            item.as_table()
                .and_then(|table| codex_section_from_table(id, table, model.clone()))
        })
        .collect()
}

fn select_ccswitch_section_for_row(
    row: &CcSwitchCodexRow,
    settings: &Value,
    global_sections: &HashMap<String, CcSwitchCodexSection>,
) -> Option<CcSwitchCodexSection> {
    let provider_id = custom_provider_id(&row.id);
    if let Some(section) = global_sections.get(&provider_id) {
        return Some(section.clone());
    }
    if let Some(section) = global_sections.get(row.id.trim()) {
        return Some(section.clone());
    }

    let config_text = settings.get("config").and_then(Value::as_str).unwrap_or("");
    let doc = config_text.parse::<DocumentMut>().ok()?;
    let model = string_value(&doc, "model");
    let active_provider = string_value(&doc, "model_provider");
    let providers = doc.get("model_providers").and_then(|item| item.as_table());

    if let Some(providers) = providers {
        for exact_id in [provider_id.as_str(), row.id.trim()] {
            if let Some(section) = providers
                .get(exact_id)
                .and_then(|item| item.as_table())
                .and_then(|table| codex_section_from_table(exact_id, table, model.clone()))
            {
                return Some(section);
            }
        }

        if active_provider.as_deref() == Some(row.id.trim())
            || active_provider.as_deref() == Some(provider_id.as_str())
        {
            if let Some(active) = active_provider.as_deref() {
                if let Some(section) = providers
                    .get(active)
                    .and_then(|item| item.as_table())
                    .and_then(|table| codex_section_from_table(active, table, model.clone()))
                {
                    return Some(section);
                }
            }
        }

        // Legacy cc-switch/custom templates often store every third-party provider
        // under `[model_providers.custom]`. Only use it when the row's own config
        // explicitly activates custom or contains no other provider identity.
        if active_provider
            .as_deref()
            .is_none_or(|active| active == "custom")
        {
            if let Some(section) = providers
                .get("custom")
                .and_then(|item| item.as_table())
                .and_then(|table| codex_section_from_table("custom", table, model.clone()))
            {
                return Some(section);
            }
        }
    }

    doc.get("base_url")
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|base_url| CcSwitchCodexSection {
            id: provider_id,
            name: None,
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            wire_api: "responses".to_string(),
            requires_openai_auth: false,
            experimental_bearer_token: experimental_bearer_token_from_doc(
                &doc,
                active_provider.as_deref(),
            ),
        })
}

pub(crate) fn build_ccswitch_codex_provider(
    row: &CcSwitchCodexRow,
    global_sections: &HashMap<String, CcSwitchCodexSection>,
) -> Option<SavedProvider> {
    let settings: Value = serde_json::from_str(&row.settings_config).ok()?;
    let section = select_ccswitch_section_for_row(row, &settings, global_sections)?;
    let api_key = ccswitch_auth_api_key(&settings).or(section.experimental_bearer_token.clone());
    Some(SavedProvider {
        id: custom_provider_id(&row.id),
        provider_name: if row.name.trim().is_empty() {
            section.name.unwrap_or_else(|| row.id.clone())
        } else {
            row.name.trim().to_string()
        },
        base_url: section.base_url,
        model: section.model.unwrap_or_else(|| "gpt-5.5".to_string()),
        api_key,
        toml_config: None,
        wire_api: section.wire_api,
        requires_openai_auth: section.requires_openai_auth,
    })
}

pub(crate) fn import_ccswitch_codex_providers_inner(path: Option<String>) -> Result<ImportResult> {
    let db = path
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or(default_ccswitch_db_path()?);

    if !db.exists() {
        let candidates = ccswitch_db_candidates()?
            .into_iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n- ");
        return Err(CodexxError::Config(format!(
            "cc-switch 数据库不存在: {}\n已检查候选路径:\n- {}",
            db.display(),
            candidates
        )));
    }

    let conn = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| {
        CodexxError::Database(format!("打开 cc-switch 数据库失败 {}: {e}", db.display()))
    })?;

    let rows_vec = read_ccswitch_codex_rows(&conn)?;

    let mut global_sections: HashMap<String, CcSwitchCodexSection> = HashMap::new();
    for row in &rows_vec {
        if is_official_ccswitch_row(row) {
            continue;
        }
        let Ok(settings) = serde_json::from_str::<Value>(&row.settings_config) else {
            continue;
        };
        let Some(config_text) = settings.get("config").and_then(Value::as_str) else {
            continue;
        };
        for section in codex_sections_from_config(config_text) {
            if !global_sections.contains_key(&section.id) {
                global_sections.insert(section.id.clone(), section);
            }
        }
    }

    let mut imported = 0usize;
    let mut added = 0usize;
    let mut updated = 0usize;
    let mut merged = 0usize;
    let mut skipped = 0usize;
    let mut warnings = Vec::new();
    let mut local_conn = open_store()?;
    let transaction = local_conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| CodexxError::Database(e.to_string()))?;
    merged += consolidate_legacy_provider_duplicates_on_connection(&transaction)?;

    for row in rows_vec {
        if is_official_ccswitch_row(&row) {
            skipped += 1;
            warnings.push(format!(
                "跳过 {} ({})：官方认证不作为第三方供应商导入",
                row.name, row.id
            ));
            continue;
        }
        match build_ccswitch_codex_provider(&row, &global_sections) {
            Some(provider) => {
                let provider = normalize_saved_provider(provider)?;
                let result =
                    upsert_ccswitch_provider_on_connection(&transaction, provider, row.id.trim())?;
                match result.kind {
                    ProviderUpsertKind::Added => added += 1,
                    ProviderUpsertKind::Updated => updated += 1,
                    ProviderUpsertKind::Merged => merged += 1,
                }
                imported += 1;
            }
            None => {
                skipped += 1;
                warnings.push(format!(
                    "跳过 {} ({})：未找到可用 config/base_url，可能是官方登录或空模板",
                    row.name, row.id
                ));
            }
        }
    }
    merged += consolidate_legacy_provider_duplicates_on_connection(&transaction)?;
    transaction
        .commit()
        .map_err(|e| CodexxError::Database(e.to_string()))?;
    let providers = list_saved_providers_on_connection(&local_conn)?;

    Ok(ImportResult {
        imported,
        added,
        updated,
        merged,
        skipped,
        warnings,
        providers,
    })
}

pub(crate) fn read_ccswitch_official_auth_inner(
    path: Option<String>,
) -> Result<Option<OfficialAuthCandidate>> {
    let db = path
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or(default_ccswitch_db_path()?);

    if !db.exists() {
        return Ok(None);
    }

    let conn = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| {
        CodexxError::Database(format!("打开 cc-switch 数据库失败 {}: {e}", db.display()))
    })?;

    let provider_columns = table_column_set(&conn, "providers")?;
    let official_filter = if provider_columns.contains("category") {
        "id = 'codex-official' OR category = 'official'"
    } else {
        // Older cc-switch databases predate the category column. The stable
        // codex-official id is still enough to identify the official row.
        "id = 'codex-official'"
    };
    let query = format!(
        "SELECT id, name, settings_config FROM providers
         WHERE app_type = 'codex' AND ({official_filter})
         ORDER BY CASE WHEN id = 'codex-official' THEN 0 ELSE 1 END
         LIMIT 1"
    );
    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| CodexxError::Database(e.to_string()))?;

    let mut rows = stmt
        .query([])
        .map_err(|e| CodexxError::Database(e.to_string()))?;

    let Some(row) = rows
        .next()
        .map_err(|e| CodexxError::Database(e.to_string()))?
    else {
        return Ok(None);
    };

    let id: String = row
        .get(0)
        .map_err(|e| CodexxError::Database(e.to_string()))?;
    let name: String = row
        .get(1)
        .map_err(|e| CodexxError::Database(e.to_string()))?;
    let settings_config: String = row
        .get(2)
        .map_err(|e| CodexxError::Database(e.to_string()))?;
    let settings: Value = serde_json::from_str(&settings_config).map_err(|e| {
        CodexxError::Database(format!("cc-switch official settings JSON 解析失败: {e}"))
    })?;

    let auth = settings
        .get("auth")
        .cloned()
        .filter(|value| value.is_object())
        .ok_or_else(|| {
            CodexxError::Database("cc-switch official provider 缺少 auth object".to_string())
        })?;

    let model = settings
        .get("config")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<DocumentMut>().ok())
        .and_then(|doc| string_value(&doc, "model"));

    let auth_json = serde_json::to_string_pretty(&auth)
        .map_err(|e| CodexxError::Database(format!("官方 auth JSON 格式化失败: {e}")))?;

    Ok(Some(OfficialAuthCandidate {
        auth_json,
        model,
        source: format!("cc-switch:{name}:{id}"),
    }))
}
