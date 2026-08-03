mod ccswitch;
mod connection;
mod live;
mod official_auth;
mod store;

use crate::error::Result;
use rusqlite::Connection;

#[cfg(test)]
pub(crate) use ccswitch::{
    build_ccswitch_codex_provider, codex_sections_from_config, is_official_ccswitch_row,
    read_ccswitch_codex_rows, CcSwitchCodexRow,
};
pub(crate) use ccswitch::{
    import_ccswitch_codex_providers_inner, read_ccswitch_official_auth_inner, ImportResult,
    OfficialAuthCandidate,
};
#[cfg(test)]
pub(crate) use connection::provider_status_result;
pub(crate) use connection::{
    fetch_provider_models_inner, test_provider_connection_inner, ProviderConnectionResult,
    ProviderModelsResult,
};
pub(crate) use live::detected_live_custom_provider;
pub(crate) use live::{
    delete_saved_provider_inner, reset_official_provider_inner, restore_official_provider_inner,
    save_active_provider_inner, save_official_config_inner, save_provider_toml_config_inner,
    switch_official_provider_inner, switch_provider_inner, OfficialConfigInput, ProviderInput,
    ProviderTomlInput,
};
#[cfg(test)]
pub(crate) use live::{
    save_provider_toml_config_with_pre_persist, switch_official_provider_with_pre_persist,
    switch_provider_with_pre_persist,
};
#[cfg(test)]
pub(crate) use official_auth::official_snapshot_path_for_test;
pub(crate) use official_auth::{
    capture_live_chatgpt_config, document_is_official, get_official_config_draft_inner,
    official_auth_available, OfficialConfigDraft,
};
#[cfg(test)]
pub(crate) use store::{
    canonical_provider_base_url, provider_by_id_on_connection, provider_identity,
    save_manual_provider_on_connection, upsert_provider_on_connection, ProviderUpsertMode,
};
pub(crate) use store::{
    consolidate_legacy_provider_duplicates_on_connection, custom_provider_id,
    delete_provider_inner, experimental_bearer_token_from_doc, is_placeholder_provider,
    list_saved_providers_inner, list_saved_providers_on_connection,
    matching_saved_provider_ids_for_live, normalize_saved_provider,
    provider_template_from_document, reserved_codex_provider_id, rollback_provider_store_inner,
    save_provider_inner, save_provider_with_rollback_inner, unique_saved_provider_id_for_live,
    upsert_ccswitch_provider_on_connection, ProviderUpsertKind, SavedProvider,
};

pub(crate) fn open_store() -> Result<Connection> {
    crate::app_db::open()
}
