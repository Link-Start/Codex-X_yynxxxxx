use super::backup::{create_provider_sync_backup, prune_provider_sync_backups};
use super::storage::{
    current_model_provider, discover_sqlite_databases, list_session_previews_with_paths,
    scan_rollouts_for_thread_ids, scan_sqlite_with_paths, SqliteDiscovery,
};
use super::transaction::{
    execute_provider_sync_mutation, mutation_error, prepare_sqlite_updates, rollback_mutation,
    rollback_open_transactions, MutationJournal, MutationPoint,
};
use super::types::{RolloutScan, SessionSyncResult, SessionSyncStatus, SqliteScan};
use crate::error::{CodexxError, Result};
use crate::file_io::{ensure_directory, io_err};
use crate::resolve_codex_dir;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;

const SHARED_SESSION_PROVIDER: &str = "custom";

fn live_route_failure(codex_dir: &Path) -> Option<String> {
    match current_model_provider(codex_dir, None) {
        Ok(provider) if provider.trim() == SHARED_SESSION_PROVIDER => None,
        Ok(provider) => Some(format!(
            "当前 Codex 配置的 model_provider 为 {provider:?}，未路由到共享 custom 会话；已停止同步，请先重新启用官方配置或供应商。"
        )),
        Err(error) => Some(format!("无法验证当前 Codex 配置的 model_provider: {error}")),
    }
}

fn scan_failure_error(failures: &[String]) -> CodexxError {
    CodexxError::Config(
        failures
            .first()
            .cloned()
            .unwrap_or_else(|| "无法确认当前会话同步状态。".to_string()),
    )
}

fn scan_provider_buckets(
    codex_dir: &Path,
    target_provider: &str,
    sqlite: &SqliteScan,
) -> Result<RolloutScan> {
    let mut rollouts = scan_rollouts_for_thread_ids(
        codex_dir,
        target_provider,
        &sqlite.thread_ids,
        &sqlite.rollout_paths_by_thread_id,
    )?;
    // Provider synchronization must not repair or rewrite the independent cwd index.
    rollouts.cwd_by_thread_id.clear();
    Ok(rollouts)
}

pub(crate) fn session_sync_status_inner(
    config_dir: Option<String>,
    _target_provider: Option<String>,
) -> Result<SessionSyncStatus> {
    let codex_dir = resolve_codex_dir(config_dir)?;
    let target = SHARED_SESSION_PROVIDER.to_string();
    let discovery = discover_sqlite_databases(&codex_dir);
    session_sync_status_with_discovery(&codex_dir, target, &discovery)
}

pub(super) fn session_sync_status_with_discovery(
    codex_dir: &Path,
    _target: String,
    discovery: &SqliteDiscovery,
) -> Result<SessionSyncStatus> {
    let target = SHARED_SESSION_PROVIDER.to_string();
    let mut scan_failures = Vec::new();
    if let Some(failure) = live_route_failure(codex_dir) {
        scan_failures.push(failure);
    }
    scan_failures.extend(discovery.active_scan_failures.iter().cloned());
    let sqlite =
        match scan_sqlite_with_paths(&discovery.active_paths, &RolloutScan::default(), &target) {
            Ok(sqlite) => sqlite,
            Err(error) => {
                scan_failures.push(format!("无法扫描当前活动会话数据库: {error}"));
                Default::default()
            }
        };
    scan_failures.extend(sqlite.scan_failures.iter().cloned());
    if !discovery.active_paths.is_empty() && sqlite.sqlite_dbs != discovery.active_paths.len() {
        scan_failures.push("当前活动会话数据库未被完整扫描。".to_string());
    }
    let rollouts = scan_provider_buckets(codex_dir, &target, &sqlite)?;
    scan_failures.extend(rollouts.scan_failures.iter().cloned());
    if discovery.active_paths.is_empty()
        && (!discovery.thread_paths.is_empty() || rollouts.discovered_rollout_files > 0)
    {
        scan_failures.push(
            "未找到当前活动会话数据库；旧数据库和单独的 JSONL 不会被当作客户端活动会话。"
                .to_string(),
        );
    }
    let session_limit = sqlite.sqlite_threads.clamp(50, 1000);
    let (sessions, session_warnings) = match list_session_previews_with_paths(
        &discovery.active_paths,
        &rollouts,
        &target,
        session_limit,
    ) {
        Ok(result) => result,
        Err(error) => {
            scan_failures.push(format!("无法读取当前活动会话列表: {error}"));
            (Vec::new(), Vec::new())
        }
    };
    scan_failures.extend(session_warnings);
    let mut seen_failures = HashSet::new();
    scan_failures.retain(|failure| seen_failures.insert(failure.clone()));
    let mut warnings = rollouts.warnings;
    warnings.extend(sqlite.warnings);
    let scan_complete = scan_failures.is_empty();
    let mismatched_sessions = sqlite
        .mismatched_thread_ids
        .union(&rollouts.mismatched_thread_ids)
        .count();
    Ok(SessionSyncStatus {
        codex_dir: codex_dir.display().to_string(),
        target_provider: target,
        rollout_files: rollouts.rollout_files,
        session_meta_count: rollouts.session_meta_count,
        mismatched_rollouts: rollouts.mismatched_rollouts,
        mismatched_session_meta: rollouts.mismatched_session_meta,
        sqlite_dbs: sqlite.sqlite_dbs,
        sqlite_threads: sqlite.sqlite_threads,
        top_level_threads: sqlite.top_level_threads,
        subagent_threads: sqlite.subagent_threads,
        mismatched_threads: sqlite.mismatched_threads,
        mismatched_sessions,
        needs_sync: mismatched_sessions > 0,
        scan_complete,
        scan_failures,
        backup_dir: None,
        warnings,
        sessions,
    })
}

pub(super) struct SessionMaintenanceLock {
    file: fs::File,
}

impl Drop for SessionMaintenanceLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub(super) fn acquire_session_maintenance_lock(codex_dir: &Path) -> Result<SessionMaintenanceLock> {
    let tmp_dir = codex_dir.join("tmp");
    ensure_directory(&tmp_dir)?;
    let legacy_lock = tmp_dir.join("provider-sync.lock");
    if legacy_lock.exists() {
        return Err(CodexxError::Config(format!(
            "会话维护正在进行: {}",
            legacy_lock.display()
        )));
    }
    let path = tmp_dir.join("session-maintenance.lock");
    if path.is_dir() {
        return Err(CodexxError::Config(format!(
            "检测到旧版会话维护锁，请确认没有其他 Codex-X 正在维护会话后删除: {}",
            path.display()
        )));
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| io_err(&path, e))?;
    file.try_lock()
        .map_err(|_| CodexxError::Config(format!("会话维护正在进行: {}", path.display())))?;
    file.set_len(0).map_err(|e| io_err(&path, e))?;
    writeln!(file, "pid={}", std::process::id()).map_err(|e| io_err(&path, e))?;
    file.sync_all().map_err(|e| io_err(&path, e))?;
    Ok(SessionMaintenanceLock { file })
}

pub(crate) fn sync_sessions_provider_inner(
    config_dir: Option<String>,
    target_provider: Option<String>,
) -> Result<SessionSyncResult> {
    sync_sessions_provider_with_hook(config_dir, target_provider, |_| Ok(()))
}

pub(super) fn sync_sessions_provider_with_hook<F>(
    config_dir: Option<String>,
    _target_provider: Option<String>,
    mut hook: F,
) -> Result<SessionSyncResult>
where
    F: FnMut(MutationPoint) -> Result<()>,
{
    let codex_dir = resolve_codex_dir(config_dir)?;
    ensure_directory(&codex_dir)?;
    let target_provider = SHARED_SESSION_PROVIDER.to_string();
    let _maintenance_lock = acquire_session_maintenance_lock(&codex_dir)?;
    let discovery = discover_sqlite_databases(&codex_dir);
    let initial_status =
        session_sync_status_with_discovery(&codex_dir, target_provider.clone(), &discovery)?;
    if !initial_status.scan_complete {
        return Err(scan_failure_error(&initial_status.scan_failures));
    }
    let sqlite = scan_sqlite_with_paths(
        &discovery.active_paths,
        &RolloutScan::default(),
        &target_provider,
    )?;
    if !sqlite.scan_failures.is_empty() {
        return Err(scan_failure_error(&sqlite.scan_failures));
    }
    let rollouts = scan_provider_buckets(&codex_dir, &target_provider, &sqlite)?;
    if !rollouts.scan_failures.is_empty() {
        return Err(scan_failure_error(&rollouts.scan_failures));
    }
    if let Some(failure) = live_route_failure(&codex_dir) {
        return Err(CodexxError::Config(failure));
    }
    if rollouts.changes.is_empty() && sqlite.mismatched_threads == 0 {
        return Ok(SessionSyncResult {
            status: initial_status,
            updated_rollouts: 0,
            updated_threads: 0,
            backup_dir: String::new(),
        });
    }

    let changed_rollouts = rollouts
        .changes
        .iter()
        .map(|change| change.path.clone())
        .collect::<Vec<_>>();
    let mut pending_sqlite = prepare_sqlite_updates(&discovery.active_paths)?;
    let sqlite_snapshot_paths = pending_sqlite
        .iter()
        .map(|update| update.path().to_path_buf())
        .collect::<Vec<_>>();
    let backup = match create_provider_sync_backup(
        &codex_dir,
        &target_provider,
        &changed_rollouts,
        &sqlite_snapshot_paths,
    ) {
        Ok(backup) => backup,
        Err(error) => {
            rollback_open_transactions(&mut pending_sqlite);
            return Err(error);
        }
    };
    let mut journal = MutationJournal::default();
    let mutation = execute_provider_sync_mutation(
        &rollouts,
        &mut pending_sqlite,
        &target_provider,
        &mut journal,
        &mut hook,
    );
    let mutation = match mutation {
        Ok(result) => result,
        Err(error) => {
            let recovery_errors = rollback_mutation(&journal, &mut pending_sqlite);
            return Err(mutation_error(error, recovery_errors));
        }
    };

    let prune_warning = prune_provider_sync_backups(&codex_dir).err();
    let mut status = session_sync_status_with_discovery(&codex_dir, target_provider, &discovery)
        .map_err(|error| {
            CodexxError::Config(format!(
                "同步已完成，但刷新会话列表失败，请重新进入页面：{error}"
            ))
        })?;
    status.backup_dir = Some(backup.dir.display().to_string());
    if prune_warning.is_some() {
        status
            .warnings
            .push("同步已完成，但旧备份暂未清理。".to_string());
    }
    if !mutation.skipped_rollouts.is_empty() {
        status.warnings.push(format!(
            "有 {} 个会话正在使用，已跳过；退出 Codex 后再同步即可。",
            mutation.skipped_rollouts.len()
        ));
    }
    Ok(SessionSyncResult {
        status,
        updated_rollouts: mutation.applied_rollouts,
        updated_threads: mutation.sqlite_updates.total(),
        backup_dir: backup.dir.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_codex_dir(label: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "codex-x-session-sync-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test Codex directory");
        path
    }

    fn write_config(codex_dir: &Path, provider: &str) {
        fs::write(
            codex_dir.join("config.toml"),
            format!("model_provider = {provider:?}\n"),
        )
        .expect("write Codex config");
    }

    fn create_thread_database(path: &Path, id: &str, provider: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create SQLite parent");
        }
        let conn = Connection::open(path).expect("create session database");
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                model_provider TEXT NOT NULL,
                title TEXT
             );",
        )
        .expect("create threads table");
        conn.execute(
            "INSERT INTO threads (id, model_provider, title) VALUES (?1, ?2, 'test')",
            (id, provider),
        )
        .expect("insert thread");
    }

    fn create_thread_database_with_rollout(path: &Path, id: &str, provider: &str, rollout: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create SQLite parent");
        }
        let conn = Connection::open(path).expect("create session database");
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                model_provider TEXT NOT NULL,
                title TEXT,
                rollout_path TEXT
             );",
        )
        .expect("create threads table with rollout path");
        conn.execute(
            "INSERT INTO threads (id, model_provider, title, rollout_path)
             VALUES (?1, ?2, 'test', ?3)",
            (id, provider, rollout.display().to_string()),
        )
        .expect("insert thread with rollout path");
    }

    fn thread_provider(path: &Path, id: &str) -> String {
        Connection::open(path)
            .expect("open session database")
            .query_row(
                "SELECT model_provider FROM threads WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .expect("read thread provider")
    }

    fn write_rollout(codex_dir: &Path, id: &str, provider: &str) -> std::path::PathBuf {
        let path = codex_dir.join(format!("sessions/rollout-test-{id}.jsonl"));
        write_rollout_at(&path, id, provider);
        path
    }

    fn write_rollout_at(path: &Path, id: &str, provider: &str) {
        fs::create_dir_all(path.parent().expect("rollout parent")).expect("create rollout parent");
        fs::write(
            path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"model_provider\":\"{provider}\"}}}}\n"
            ),
        )
        .expect("write rollout");
    }

    #[test]
    fn live_provider_must_route_to_custom_before_status_can_be_complete() {
        let codex_dir = temp_codex_dir("live-provider-gate");
        write_config(&codex_dir, "openai");

        let status = session_sync_status_inner(Some(codex_dir.display().to_string()), None)
            .expect("read blocked status");
        assert!(!status.scan_complete);
        assert!(!status.needs_sync);
        assert!(status
            .scan_failures
            .iter()
            .any(|failure| failure.contains("model_provider") && failure.contains("openai")));

        let error = sync_sessions_provider_inner(Some(codex_dir.display().to_string()), None)
            .expect_err("non-custom live route must block synchronization");
        assert!(error.to_string().contains("未路由到共享 custom 会话"));

        fs::remove_dir_all(codex_dir).expect("remove test directory");
    }

    #[test]
    fn unreadable_jsonl_cannot_report_all_sessions_synced() {
        let codex_dir = temp_codex_dir("unreadable-jsonl");
        write_config(&codex_dir, SHARED_SESSION_PROVIDER);
        let id = "019f6000-0000-7000-8000-000000000501";
        create_thread_database(&codex_dir.join("state_5.sqlite"), id, "custom");
        let rollout = codex_dir.join(format!("sessions/rollout-test-{id}.jsonl"));
        fs::create_dir_all(rollout.parent().expect("rollout parent"))
            .expect("create rollout parent");
        fs::write(&rollout, "not-json\n").expect("write malformed rollout");

        let status = session_sync_status_inner(Some(codex_dir.display().to_string()), None)
            .expect("read incomplete status");
        assert!(!status.scan_complete);
        assert!(!status.needs_sync);
        assert!(status
            .scan_failures
            .iter()
            .any(|failure| failure.contains("无法解析的 JSON")));
        sync_sessions_provider_inner(Some(codex_dir.display().to_string()), None)
            .expect_err("incomplete JSONL scan must block synchronization");

        fs::remove_dir_all(codex_dir).expect("remove test directory");
    }

    #[test]
    fn malformed_standard_orphan_rollout_does_not_block_active_sessions() {
        let codex_dir = temp_codex_dir("malformed-standard-orphan");
        write_config(&codex_dir, SHARED_SESSION_PROVIDER);
        let active_id = "019f6000-0000-7000-8000-000000000502";
        let orphan_id = "019f6000-0000-7000-8000-000000000503";
        create_thread_database(
            &codex_dir.join("state_5.sqlite"),
            active_id,
            SHARED_SESSION_PROVIDER,
        );
        write_rollout(&codex_dir, active_id, SHARED_SESSION_PROVIDER);
        fs::write(
            codex_dir.join(format!("sessions/rollout-test-{orphan_id}.jsonl")),
            b"\xff",
        )
        .expect("write invalid orphan rollout");

        let status = session_sync_status_inner(Some(codex_dir.display().to_string()), None)
            .expect("scan active sessions only");
        assert!(status.scan_complete, "{:?}", status.scan_failures);
        assert_eq!(status.rollout_files, 1);
        assert_eq!(status.session_meta_count, 1);
        assert!(!status.needs_sync);

        fs::remove_dir_all(codex_dir).expect("remove test directory");
    }

    #[test]
    fn malformed_unreferenced_nonstandard_rollout_does_not_block_active_sessions() {
        let codex_dir = temp_codex_dir("malformed-unreferenced-rollout");
        write_config(&codex_dir, SHARED_SESSION_PROVIDER);
        let active_id = "019f6000-0000-7000-8000-000000000504";
        create_thread_database(
            &codex_dir.join("state_5.sqlite"),
            active_id,
            SHARED_SESSION_PROVIDER,
        );
        write_rollout(&codex_dir, active_id, SHARED_SESSION_PROVIDER);
        fs::write(
            codex_dir.join("sessions/rollout-imported-orphan.jsonl"),
            b"\xff",
        )
        .expect("write invalid unreferenced rollout");

        let status = session_sync_status_inner(Some(codex_dir.display().to_string()), None)
            .expect("scan referenced sessions only");
        assert!(status.scan_complete, "{:?}", status.scan_failures);
        assert_eq!(status.rollout_files, 1);
        assert_eq!(status.session_meta_count, 1);

        fs::remove_dir_all(codex_dir).expect("remove test directory");
    }

    #[test]
    fn sqlite_referenced_nonstandard_rollout_is_synchronized() {
        let codex_dir = temp_codex_dir("referenced-nonstandard-rollout");
        write_config(&codex_dir, SHARED_SESSION_PROVIDER);
        let id = "019f6000-0000-7000-8000-000000000505";
        let rollout = codex_dir.join("sessions/rollout-imported-name.jsonl");
        write_rollout_at(&rollout, id, "openai");
        let database = codex_dir.join("state_5.sqlite");
        create_thread_database_with_rollout(&database, id, "openai", &rollout);

        let status = session_sync_status_inner(Some(codex_dir.display().to_string()), None)
            .expect("scan referenced imported rollout");
        assert!(status.scan_complete, "{:?}", status.scan_failures);
        assert!(status.needs_sync);
        assert_eq!(status.rollout_files, 1);
        assert_eq!(status.mismatched_sessions, 1);

        let result = sync_sessions_provider_inner(
            Some(codex_dir.display().to_string()),
            Some(SHARED_SESSION_PROVIDER.to_string()),
        )
        .expect("synchronize referenced imported rollout");
        assert_eq!(result.updated_rollouts, 1);
        assert_eq!(result.updated_threads, 1);
        assert_eq!(thread_provider(&database, id), SHARED_SESSION_PROVIDER);
        assert!(fs::read_to_string(&rollout)
            .expect("read synchronized imported rollout")
            .contains("\"model_provider\":\"custom\""));

        fs::remove_dir_all(codex_dir).expect("remove test directory");
    }

    #[test]
    fn sqlite_referenced_rollout_with_a_different_session_id_blocks_sync() {
        let codex_dir = temp_codex_dir("referenced-rollout-id-mismatch");
        write_config(&codex_dir, SHARED_SESSION_PROVIDER);
        let sqlite_id = "019f6000-0000-7000-8000-000000000506";
        let rollout_id = "019f6000-0000-7000-8000-000000000507";
        let rollout = codex_dir.join("sessions/rollout-imported-name.jsonl");
        write_rollout_at(&rollout, rollout_id, "openai");
        let database = codex_dir.join("state_5.sqlite");
        create_thread_database_with_rollout(&database, sqlite_id, "openai", &rollout);

        let status = session_sync_status_inner(Some(codex_dir.display().to_string()), None)
            .expect("scan mismatched referenced rollout");
        assert!(!status.scan_complete);
        assert!(status
            .scan_failures
            .iter()
            .any(|failure| failure.contains("线程 ID 不一致")));

        let error = sync_sessions_provider_inner(
            Some(codex_dir.display().to_string()),
            Some(SHARED_SESSION_PROVIDER.to_string()),
        )
        .expect_err("mismatched referenced rollout must block synchronization");
        assert!(error.to_string().contains("线程 ID 不一致"));
        assert_eq!(thread_provider(&database, sqlite_id), "openai");
        assert!(fs::read_to_string(&rollout)
            .expect("read unchanged rollout")
            .contains("\"model_provider\":\"openai\""));

        fs::remove_dir_all(codex_dir).expect("remove test directory");
    }

    #[test]
    fn missing_sqlite_referenced_rollout_blocks_sync_without_using_uuid_fallback() {
        let codex_dir = temp_codex_dir("missing-referenced-rollout");
        write_config(&codex_dir, SHARED_SESSION_PROVIDER);
        let id = "019f6000-0000-7000-8000-000000000506";
        let missing = codex_dir.join("sessions/missing/rollout-selected.jsonl");
        let duplicate = write_rollout(&codex_dir, id, "openai");
        create_thread_database_with_rollout(
            &codex_dir.join("state_5.sqlite"),
            id,
            SHARED_SESSION_PROVIDER,
            &missing,
        );

        let status = session_sync_status_inner(Some(codex_dir.display().to_string()), None)
            .expect("scan missing referenced rollout");
        assert!(!status.scan_complete);
        assert!(!status.needs_sync);
        assert_eq!(status.rollout_files, 0);
        assert!(status
            .scan_failures
            .iter()
            .any(|failure| failure.contains("会话文件不存在")));
        sync_sessions_provider_inner(Some(codex_dir.display().to_string()), None)
            .expect_err("missing referenced rollout must block synchronization");
        assert!(fs::read_to_string(duplicate)
            .expect("read untouched UUID fallback")
            .contains("\"model_provider\":\"openai\""));

        fs::remove_dir_all(codex_dir).expect("remove test directory");
    }

    #[test]
    fn unreadable_sqlite_referenced_rollout_cannot_report_synced() {
        let codex_dir = temp_codex_dir("unreadable-referenced-rollout");
        write_config(&codex_dir, SHARED_SESSION_PROVIDER);
        let id = "019f6000-0000-7000-8000-000000000507";
        let rollout = codex_dir.join("sessions/rollout-unreadable.jsonl");
        fs::create_dir_all(rollout.parent().expect("rollout parent"))
            .expect("create rollout parent");
        fs::write(&rollout, b"\xff").expect("write invalid UTF-8 rollout");
        create_thread_database_with_rollout(
            &codex_dir.join("state_5.sqlite"),
            id,
            SHARED_SESSION_PROVIDER,
            &rollout,
        );

        let status = session_sync_status_inner(Some(codex_dir.display().to_string()), None)
            .expect("scan unreadable referenced rollout");
        assert!(!status.scan_complete);
        assert!(!status.needs_sync);
        assert_eq!(status.rollout_files, 1);
        assert!(status
            .scan_failures
            .iter()
            .any(|failure| failure.contains("无法读取会话文件")));
        sync_sessions_provider_inner(Some(codex_dir.display().to_string()), None)
            .expect_err("unreadable referenced rollout must block synchronization");

        fs::remove_dir_all(codex_dir).expect("remove test directory");
    }

    #[test]
    fn sqlite_rollout_path_excludes_other_files_with_the_same_thread_id() {
        let codex_dir = temp_codex_dir("referenced-rollout-excludes-duplicates");
        write_config(&codex_dir, SHARED_SESSION_PROVIDER);
        let id = "019f6000-0000-7000-8000-000000000508";
        let selected = codex_dir.join("sessions/rollout-selected-name.jsonl");
        write_rollout_at(&selected, id, "openai");
        let duplicate = write_rollout(&codex_dir, id, "openai");
        let database = codex_dir.join("state_5.sqlite");
        create_thread_database_with_rollout(&database, id, "openai", &selected);

        let status = session_sync_status_inner(Some(codex_dir.display().to_string()), None)
            .expect("scan only the SQLite-selected rollout");
        assert!(status.scan_complete, "{:?}", status.scan_failures);
        assert_eq!(status.rollout_files, 1);
        assert_eq!(status.mismatched_rollouts, 1);

        let result = sync_sessions_provider_inner(Some(codex_dir.display().to_string()), None)
            .expect("synchronize only the SQLite-selected rollout");
        assert_eq!(result.updated_rollouts, 1);
        assert_eq!(result.updated_threads, 1);
        assert_eq!(thread_provider(&database, id), SHARED_SESSION_PROVIDER);
        assert!(fs::read_to_string(&selected)
            .expect("read selected rollout")
            .contains("\"model_provider\":\"custom\""));
        assert!(fs::read_to_string(&duplicate)
            .expect("read duplicate rollout")
            .contains("\"model_provider\":\"openai\""));

        fs::remove_dir_all(codex_dir).expect("remove test directory");
    }

    #[test]
    fn sqlite_referenced_rollout_outside_session_storage_is_rejected() {
        let codex_dir = temp_codex_dir("outside-referenced-rollout");
        let outside_dir = temp_codex_dir("outside-referenced-rollout-target");
        write_config(&codex_dir, SHARED_SESSION_PROVIDER);
        let id = "019f6000-0000-7000-8000-000000000509";
        let rollout = outside_dir.join("rollout-external.jsonl");
        write_rollout_at(&rollout, id, "openai");
        create_thread_database_with_rollout(
            &codex_dir.join("state_5.sqlite"),
            id,
            SHARED_SESSION_PROVIDER,
            &rollout,
        );

        let status = session_sync_status_inner(Some(codex_dir.display().to_string()), None)
            .expect("reject external referenced rollout");
        assert!(!status.scan_complete);
        assert!(!status.needs_sync);
        assert_eq!(status.rollout_files, 0);
        assert!(status
            .scan_failures
            .iter()
            .any(|failure| failure.contains("超出 Codex 会话目录")));
        sync_sessions_provider_inner(Some(codex_dir.display().to_string()), None)
            .expect_err("external referenced rollout must block synchronization");
        assert!(fs::read_to_string(&rollout)
            .expect("read untouched external rollout")
            .contains("\"model_provider\":\"openai\""));

        fs::remove_dir_all(codex_dir).expect("remove test directory");
        fs::remove_dir_all(outside_dir).expect("remove external test directory");
    }

    #[test]
    fn unreadable_active_sqlite_cannot_report_all_sessions_synced() {
        let codex_dir = temp_codex_dir("unreadable-active-sqlite");
        write_config(&codex_dir, SHARED_SESSION_PROVIDER);
        fs::write(codex_dir.join("state_5.sqlite"), b"SQLite format 3\0")
            .expect("write truncated SQLite");

        let status = session_sync_status_inner(Some(codex_dir.display().to_string()), None)
            .expect("read incomplete status");
        assert!(!status.scan_complete);
        assert_eq!(status.sqlite_threads, 0);
        assert!(status
            .scan_failures
            .iter()
            .any(|failure| failure.contains("活动会话数据库")));
        sync_sessions_provider_inner(Some(codex_dir.display().to_string()), None)
            .expect_err("unreadable active SQLite must block synchronization");

        fs::remove_dir_all(codex_dir).expect("remove test directory");
    }

    #[test]
    fn legacy_sqlite_is_not_counted_or_modified() {
        let codex_dir = temp_codex_dir("active-only");
        write_config(&codex_dir, SHARED_SESSION_PROVIDER);
        let active = codex_dir.join("state_5.sqlite");
        let legacy = codex_dir.join("sqlite/state_5.sqlite");
        let active_id = "019f6000-0000-7000-8000-000000000511";
        let legacy_id = "019f6000-0000-7000-8000-000000000512";
        create_thread_database(&active, active_id, "openai");
        create_thread_database(&legacy, legacy_id, "openai");

        let status = session_sync_status_inner(Some(codex_dir.display().to_string()), None)
            .expect("read active status");
        assert!(status.scan_complete);
        assert_eq!(status.sqlite_dbs, 1);
        assert_eq!(status.sqlite_threads, 1);
        assert_eq!(status.mismatched_threads, 1);
        assert_eq!(status.sessions.len(), 1);
        assert_eq!(status.sessions[0].id, active_id);

        let result = sync_sessions_provider_inner(Some(codex_dir.display().to_string()), None)
            .expect("sync active database");
        assert_eq!(result.updated_threads, 1);
        assert_eq!(thread_provider(&active, active_id), "custom");
        assert_eq!(thread_provider(&legacy, legacy_id), "openai");

        fs::remove_dir_all(codex_dir).expect("remove test directory");
    }

    #[test]
    fn configured_sqlite_home_is_the_only_active_database_root() {
        let codex_dir = temp_codex_dir("configured-sqlite-home");
        fs::write(
            codex_dir.join("config.toml"),
            "model_provider = \"custom\"\nsqlite_home = \"active-sqlite\"\n",
        )
        .expect("write configured SQLite home");
        let configured = codex_dir.join("active-sqlite/state_5.sqlite");
        let root_copy = codex_dir.join("state_10.sqlite");
        let configured_id = "019f6000-0000-7000-8000-000000000515";
        let root_id = "019f6000-0000-7000-8000-000000000516";
        create_thread_database(&configured, configured_id, "openai");
        create_thread_database(&root_copy, root_id, "openai");

        let status = session_sync_status_inner(Some(codex_dir.display().to_string()), None)
            .expect("read configured active status");
        assert!(status.scan_complete);
        assert_eq!(status.sqlite_threads, 1);
        assert_eq!(status.sessions[0].id, configured_id);

        let result = sync_sessions_provider_inner(Some(codex_dir.display().to_string()), None)
            .expect("sync configured active database");
        assert_eq!(result.updated_threads, 1);
        assert_eq!(thread_provider(&configured, configured_id), "custom");
        assert_eq!(thread_provider(&root_copy, root_id), "openai");

        fs::remove_dir_all(codex_dir).expect("remove test directory");
    }

    #[test]
    fn legacy_only_database_is_not_treated_as_active() {
        let codex_dir = temp_codex_dir("legacy-only");
        write_config(&codex_dir, SHARED_SESSION_PROVIDER);
        let legacy = codex_dir.join("sqlite/state_5.sqlite");
        let id = "019f6000-0000-7000-8000-000000000521";
        create_thread_database(&legacy, id, "openai");

        let status = session_sync_status_inner(Some(codex_dir.display().to_string()), None)
            .expect("read legacy-only status");
        assert!(!status.scan_complete);
        assert_eq!(status.sqlite_threads, 0);
        assert!(status.sessions.is_empty());
        sync_sessions_provider_inner(Some(codex_dir.display().to_string()), None)
            .expect_err("legacy-only database must not be synchronized");
        assert_eq!(thread_provider(&legacy, id), "openai");

        fs::remove_dir_all(codex_dir).expect("remove test directory");
    }

    #[test]
    fn orphan_rollout_is_not_counted_or_modified() {
        let codex_dir = temp_codex_dir("orphan-rollout");
        write_config(&codex_dir, SHARED_SESSION_PROVIDER);
        let active_id = "019f6000-0000-7000-8000-000000000531";
        let orphan_id = "019f6000-0000-7000-8000-000000000532";
        create_thread_database(
            &codex_dir.join("state_5.sqlite"),
            active_id,
            SHARED_SESSION_PROVIDER,
        );
        let orphan = write_rollout(&codex_dir, orphan_id, "openai");

        let status = session_sync_status_inner(Some(codex_dir.display().to_string()), None)
            .expect("read active-only status");
        assert!(status.scan_complete);
        assert!(!status.needs_sync);
        assert_eq!(status.mismatched_sessions, 0);
        assert_eq!(status.mismatched_rollouts, 0);

        let result = sync_sessions_provider_inner(Some(codex_dir.display().to_string()), None)
            .expect("orphan rollout does not require synchronization");
        assert_eq!(result.updated_rollouts, 0);
        assert!(fs::read_to_string(orphan)
            .expect("read orphan rollout")
            .contains("\"model_provider\":\"openai\""));

        fs::remove_dir_all(codex_dir).expect("remove test directory");
    }

    #[test]
    fn mismatch_count_is_the_union_of_active_session_ids() {
        let codex_dir = temp_codex_dir("mismatch-union");
        write_config(&codex_dir, SHARED_SESSION_PROVIDER);
        let database = codex_dir.join("state_5.sqlite");
        let sqlite_mismatch = "019f6000-0000-7000-8000-000000000541";
        let rollout_mismatch = "019f6000-0000-7000-8000-000000000542";
        create_thread_database(&database, sqlite_mismatch, "openai");
        Connection::open(&database)
            .expect("open active database")
            .execute(
                "INSERT INTO threads (id, model_provider, title) VALUES (?1, 'custom', 'test')",
                [rollout_mismatch],
            )
            .expect("insert second active thread");
        write_rollout(&codex_dir, rollout_mismatch, "openai");

        let status = session_sync_status_inner(Some(codex_dir.display().to_string()), None)
            .expect("read mismatch union");
        assert!(status.scan_complete);
        assert_eq!(status.mismatched_threads, 1);
        assert_eq!(status.mismatched_rollouts, 1);
        assert_eq!(status.mismatched_sessions, 2);
        assert!(status
            .sessions
            .iter()
            .find(|session| session.id == rollout_mismatch)
            .is_some_and(|session| session.needs_sync));

        let result = sync_sessions_provider_inner(Some(codex_dir.display().to_string()), None)
            .expect("sync mismatch union");
        assert_eq!(result.updated_threads, 1);
        assert_eq!(result.updated_rollouts, 1);
        assert_eq!(result.status.mismatched_sessions, 0);

        fs::remove_dir_all(codex_dir).expect("remove test directory");
    }

    #[test]
    fn global_state_drift_does_not_trigger_provider_sync() {
        let codex_dir = temp_codex_dir("ignore-global-state-drift");
        fs::write(
            codex_dir.join("config.toml"),
            "model_provider = \"custom\"\n",
        )
        .expect("write official config");
        let global_state = codex_dir.join(".codex-global-state.json");
        let original = br#"{"electron-saved-workspace-roots":"/tmp/project"}"#;
        fs::write(&global_state, original).expect("write global state drift");

        let status = session_sync_status_inner(
            Some(codex_dir.display().to_string()),
            Some("custom".to_string()),
        )
        .expect("read shared session status");
        assert_eq!(status.target_provider, SHARED_SESSION_PROVIDER);
        assert!(!status.needs_sync);

        let result = sync_sessions_provider_inner(
            Some(codex_dir.display().to_string()),
            Some("custom".to_string()),
        )
        .expect("global state drift is not a provider migration");
        assert_eq!(result.status.target_provider, SHARED_SESSION_PROVIDER);
        assert_eq!(result.updated_rollouts, 0);
        assert_eq!(result.updated_threads, 0);
        assert!(result.backup_dir.is_empty());
        assert_eq!(
            fs::read(&global_state).expect("read unchanged state"),
            original
        );
        assert!(!codex_dir.join(".codex-global-state.json.bak").exists());

        fs::remove_dir_all(codex_dir).expect("remove test directory");
    }
}
