use crate::{
    favorites::{FavoriteLibrary, FavoriteTestJob},
    model::{ScanConfig, ScanRun, CLOUDFLARE_CANDIDATE_COUNT},
};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::Path};

const CANDIDATE_COVERAGE_VERSION: u32 = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedState {
    #[serde(default)]
    pub candidate_coverage_version: u32,
    #[serde(default)]
    pub config: ScanConfig,
    pub last_run: Option<ScanRun>,
    #[serde(default)]
    pub favorites: FavoriteLibrary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favorite_test: Option<FavoriteTestJob>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

impl Default for SavedState {
    fn default() -> Self {
        Self {
            candidate_coverage_version: CANDIDATE_COVERAGE_VERSION,
            config: ScanConfig::default(),
            last_run: None,
            favorites: FavoriteLibrary::default(),
            favorite_test: None,
            warning: None,
        }
    }
}

fn preserve_legacy_budget(config: Option<&mut serde_json::Value>) {
    if let Some(config) = config.and_then(serde_json::Value::as_object_mut) {
        // Before this switch existed, every recorded scan was budget-limited.
        // Keep that history truthful while active settings use the new default.
        config
            .entry("downloadBudgetEnabled")
            .or_insert(serde_json::Value::Bool(true));
    }
}

/// Recover only after preserving the original bytes in a unique sibling file.
/// A failed backup must stop startup: continuing with defaults would let a
/// later save or window close overwrite the only copy of the user's data.
pub fn load_for_startup(path: &Path) -> Result<SavedState, String> {
    let error = match load(path) {
        Ok(saved) => return Ok(saved),
        Err(error) => error,
    };
    let preserve_error =
        |detail| format!("{error}；无法备份原始设置，已停止启动以保留原文件：{detail}");
    let bytes = fs::read(path).map_err(|e| preserve_error(e.to_string()))?;
    let parent = path
        .parent()
        .ok_or_else(|| preserve_error("文件路径缺少目录".into()))?;
    let name = path
        .file_name()
        .ok_or_else(|| preserve_error("文件路径缺少名称".into()))?;
    let mut backup = tempfile::Builder::new()
        .prefix(&format!("{}.recovery-", name.to_string_lossy()))
        .suffix(".bak")
        .tempfile_in(parent)
        .map_err(|e| preserve_error(e.to_string()))?;
    backup
        .write_all(&bytes)
        .and_then(|_| backup.flush())
        .and_then(|_| backup.as_file().sync_all())
        .map_err(|e| preserve_error(e.to_string()))?;
    let (_, backup_path) = backup
        .keep()
        .map_err(|e| preserve_error(e.error.to_string()))?;
    Ok(SavedState {
        warning: Some(format!(
            "{error}；原始设置已备份至 {}，本次使用默认设置",
            backup_path.display()
        )),
        ..SavedState::default()
    })
}

pub fn load(path: &Path) -> Result<SavedState, String> {
    if !path.exists() {
        return Ok(SavedState::default());
    }
    let bytes = fs::read(path).map_err(|e| format!("无法读取本地设置：{e}"))?;
    let mut json: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("本地设置格式错误：{e}"))?;
    preserve_legacy_budget(json.pointer_mut("/lastRun/config"));
    preserve_legacy_budget(json.pointer_mut("/favoriteTest/run/config"));
    if let Some(entries) = json
        .pointer_mut("/favorites/entries")
        .and_then(serde_json::Value::as_array_mut)
    {
        for entry in entries {
            preserve_legacy_budget(entry.get_mut("config"));
        }
    }
    if let Some(config) = json
        .get_mut("lastRun")
        .and_then(|run| run.get_mut("config"))
        .and_then(serde_json::Value::as_object_mut)
    {
        // Do not fabricate a 200 ms gate for a scan that predates this setting.
        config.entry("maxTcpMs").or_insert(serde_json::Value::Null);
        config
            .entry("stabilityAttempts")
            .or_insert(serde_json::Value::Null);
        config
            .entry("stabilityLatencyMetric")
            .or_insert(serde_json::Value::String("max".into()));
        config
            .entry("tcpLatencyScreening")
            .or_insert(serde_json::Value::Bool(true));
        // Old scans may allow more than one failed connection and interleaved
        // stability/download batches. Do not apply the new two-failure rule to
        // their saved measurements.
        config
            .entry("stabilityFirst")
            .or_insert(serde_json::Value::Bool(false));
        // Old scans used a fixed per-region cap. Preserve that fact while
        // active settings without a recorded mode use the new adaptive default.
        config
            .entry("precisionMode")
            .or_insert(serde_json::Value::String("limited".into()));
        config
            .entry("candidatePoolSize")
            .or_insert(serde_json::Value::Null);
        // A missing cap in a historical adaptive scan meant unlimited refill.
        // Do not relabel that old queue as a capped scan.
        config
            .entry("regionalCandidateLimit")
            .or_insert(serde_json::Value::Null);
    }
    let mut saved: SavedState =
        serde_json::from_value(json).map_err(|e| format!("本地设置格式错误：{e}"))?;
    if saved.candidate_coverage_version < 1 {
        // Upgrade the former built-in default only once. A later manual cap,
        // custom source, and every historical scan retain their own settings.
        if saved.config.source == "cloudflare" && saved.config.max_candidates == 1024 {
            saved.config.max_candidates = CLOUDFLARE_CANDIDATE_COUNT;
        }
    }
    if saved.candidate_coverage_version < 2 && saved.config.regional_candidate_limit == Some(50) {
        // Reduce the former default for future scans, preserving all recorded
        // runs and any user-entered cap after this one-time migration.
        saved.config.regional_candidate_limit = Some(20);
    }
    saved.candidate_coverage_version = CANDIDATE_COVERAGE_VERSION;
    if saved.config.max_tcp_ms.is_none() {
        saved.config.max_tcp_ms = ScanConfig::default().max_tcp_ms;
    }
    if saved.config.stability_attempts.is_none() {
        saved.config.stability_attempts = ScanConfig::default().stability_attempts;
    }
    if saved.config.regional_candidate_limit.is_none() {
        saved.config.regional_candidate_limit = ScanConfig::default().regional_candidate_limit;
    }
    saved.config.tcp_latency_screening = false;
    saved.config.stability_first = true;
    if let Some(run) = saved.last_run.as_mut() {
        for result in &mut run.results {
            // Legacy records lack measurementPassed. Recover only formerly
            // eligible rows or the exact old speed-only filter, with the full
            // verification evidence still present. Hard failures never recover.
            result.measurement_passed = result.has_complete_measurement(&run.config)
                && (result.measurement_passed
                    || result.eligible
                    || result.has_only_speed_target_exclusion());
            if !run.config.accepts_initial_tcp_ms(result.tcp_ms) {
                result.eligible = false;
                result.role = Some("candidate".into());
            }
            if let Some(reason) = result.stability_exclusion(&run.config) {
                result.eligible = false;
                result.role = Some("candidate".into());
                if result.exclusion_reason.is_none() || result.has_only_speed_target_exclusion() {
                    result.exclusion_reason = Some(reason);
                }
            }
        }
        if run.status == "running" {
            run.status = "cancelled".into();
            run.progress.stage = "cancelled".into();
            run.progress.stability_batch = None;
            run.progress.precision_batch = None;
            run.progress.message = "上次关闭应用时测试中断，已保留已完成数据".into();
            run.finished_at = Some(chrono::Utc::now().to_rfc3339());
            for result in &mut run.results {
                result.role = Some("candidate".into());
            }
        }
    }
    recover_interrupted_favorite_test(&mut saved, path);
    Ok(saved)
}

fn recover_interrupted_favorite_test(saved: &mut SavedState, path: &Path) {
    let Some(job) = saved
        .favorite_test
        .as_mut()
        .filter(|job| job.run.status == "running")
    else {
        return;
    };
    job.interrupt("上次关闭应用时收藏测速中断，已保留已完成数据");
    let mut next = saved.clone();
    next.favorites
        .apply_test_results(next.favorite_test.as_ref().unwrap());
    // Persist recovery before exposing its new library version. Reopening the
    // app then reads the same terminal job instead of generating another
    // different recovered snapshot with the same revision on each load.
    let committed = next.favorites.next_revision().and_then(|revision| {
        next.favorites.revision = revision;
        next.warning = None;
        save(path, &next)
    });
    match committed {
        Ok(()) => *saved = next,
        Err(error) => {
            // Retain the interrupted job in memory for diagnosis, but preserve
            // the original library and its revision when persistence fails.
            saved.warning = Some(format!(
                "中断的收藏测速结果尚未保存到本地，原收藏数据已保留：{error}"
            ));
        }
    }
}

pub fn save(path: &Path, saved: &SavedState) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(saved).map_err(|e| e.to_string())?;
    write_atomic(path, &bytes)
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("文件路径缺少目录")?;
    fs::create_dir_all(parent).map_err(|e| format!("无法创建保存目录：{e}"))?;
    let mut file =
        tempfile::NamedTempFile::new_in(parent).map_err(|e| format!("无法创建临时文件：{e}"))?;
    file.write_all(bytes)
        .and_then(|_| file.flush())
        .map_err(|e| format!("无法写入文件：{e}"))?;
    file.as_file()
        .sync_all()
        .map_err(|e| format!("无法同步文件：{e}"))?;
    file.persist(path)
        .map_err(|e| format!("无法保存文件：{}", e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recovery_backups(path: &Path) -> Vec<std::path::PathBuf> {
        fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|entry| {
                entry
                    .extension()
                    .is_some_and(|extension| extension == "bak")
            })
            .collect()
    }

    #[test]
    fn startup_load_keeps_valid_state_and_missing_file_behavior() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let missing = load_for_startup(&path).unwrap();
        assert!(missing.warning.is_none());
        assert!(!path.exists());

        let mut saved = SavedState::default();
        saved.config.regions = vec!["JP".into()];
        save(&path, &saved).unwrap();
        let original = fs::read(&path).unwrap();
        let loaded = load_for_startup(&path).unwrap();
        assert_eq!(loaded.config.regions, ["JP"]);
        assert!(loaded.warning.is_none());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(recovery_backups(&path).is_empty());
    }

    #[test]
    fn startup_preserves_corrupt_json_before_defaults_can_be_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let original = b"{\"favorites\":\"recoverable data\",\xff";
        fs::write(&path, original).unwrap();

        let recovered = load_for_startup(&path).unwrap();
        let backups = recovery_backups(&path);
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(&backups[0]).unwrap(), original);
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(recovered
            .warning
            .as_ref()
            .unwrap()
            .contains(backups[0].to_str().unwrap()));
        assert!(recovered.favorites.entries.is_empty());

        // This is the same save that closing a default-state window performs.
        save(&path, &recovered).unwrap();
        assert!(load(&path).is_ok());
        assert_eq!(fs::read(&backups[0]).unwrap(), original);
    }

    #[test]
    fn startup_preserves_valid_json_with_invalid_state_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut json = serde_json::to_value(SavedState::default()).unwrap();
        json["favorites"]["entries"] = serde_json::json!({ "savedIp": "104.16.1.1" });
        let original = serde_json::to_vec_pretty(&json).unwrap();
        fs::write(&path, &original).unwrap();
        assert!(load(&path).is_err());

        let recovered = load_for_startup(&path).unwrap();
        let backups = recovery_backups(&path);
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(&backups[0]).unwrap(), original);
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(recovered.warning.unwrap().contains("已备份至"));
    }

    #[test]
    fn repeated_startup_recovery_never_overwrites_an_existing_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, b"first invalid state").unwrap();
        let first = load_for_startup(&path).unwrap();
        let first_backup = recovery_backups(&path).pop().unwrap();
        fs::write(&path, b"second invalid state").unwrap();
        let second = load_for_startup(&path).unwrap();
        let backups = recovery_backups(&path);
        assert_eq!(backups.len(), 2);
        assert_ne!(first.warning, second.warning);
        assert_eq!(fs::read(&first_backup).unwrap(), b"first invalid state");
        let second_backup = backups
            .iter()
            .find(|backup| **backup != first_backup)
            .unwrap();
        assert_eq!(fs::read(second_backup).unwrap(), b"second invalid state");
    }

    #[test]
    fn startup_backup_failure_returns_error_and_keeps_original_file() {
        let dir = tempfile::tempdir().unwrap();
        // This valid filename leaves no room for the unique backup suffix on
        // macOS or Windows. It reliably fails without depending on permissions.
        let path = dir.path().join("s".repeat(250));
        let original = b"invalid state that must not be overwritten";
        fs::write(&path, original).unwrap();

        let error = load_for_startup(&path).unwrap_err();
        assert!(error.contains("无法备份原始设置"));
        assert!(error.contains("已停止启动"));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    fn legacy_run() -> ScanRun {
        let mut low = crate::model::IpResult::new("104.16.1.1".parse().unwrap());
        low.region_code = "JP".into();
        low.city = "东京".into();
        low.colo = "NRT".into();
        low.observed_colos = vec!["NRT".into()];
        low.success_count = 5;
        low.attempts = 5;
        low.download_samples_mbps = vec![8.0, 9.0];
        low.download_mbps = 8.5;
        low.exclusion_reason = Some("下载 8.50 Mbps，低于日常优选目标 20.00 Mbps".into());
        let mut recommended = low.clone();
        recommended.ip = "104.16.1.2".into();
        recommended.download_samples_mbps = vec![25.0, 25.0];
        recommended.download_mbps = 25.0;
        recommended.exclusion_reason = None;
        recommended.eligible = true;
        recommended.role = Some("primary".into());
        let mut hard = low.clone();
        hard.ip = "104.16.1.3".into();
        hard.exclusion_reason = Some("HTTPS 复测失败：HTTPS 请求超时".into());
        let mut incomplete = low.clone();
        incomplete.ip = "104.16.1.4".into();
        incomplete.download_samples_mbps.truncate(1);
        let mut failed_attempt = low.clone();
        failed_attempt.ip = "104.16.1.5".into();
        failed_attempt.success_count = 4;
        let mut drift = low.clone();
        drift.ip = "104.16.1.6".into();
        drift.observed_colos.push("HKG".into());
        let mut unclassified = low.clone();
        unclassified.ip = "104.16.1.7".into();
        unclassified.exclusion_reason = None;
        ScanRun {
            id: "legacy-run".into(),
            status: "completed".into(),
            started_at: "2026-10-23T00:00:00Z".into(),
            finished_at: Some("2026-10-23T00:01:00Z".into()),
            config: ScanConfig {
                regions: vec!["JP".into()],
                max_tcp_ms: Some(200.0),
                ..Default::default()
            },
            progress: Default::default(),
            results: vec![
                low,
                recommended,
                hard,
                incomplete,
                failed_attempt,
                drift,
                unclassified,
            ],
            error: None,
        }
    }

    fn write_legacy(path: &Path, run: ScanRun) -> Vec<u8> {
        let mut json = serde_json::to_value(SavedState {
            last_run: Some(run),
            ..Default::default()
        })
        .unwrap();
        json["config"]
            .as_object_mut()
            .unwrap()
            .remove("precisionMode");
        json["lastRun"]["config"]
            .as_object_mut()
            .unwrap()
            .remove("precisionMode");
        json["config"]
            .as_object_mut()
            .unwrap()
            .remove("candidatePoolSize");
        json["lastRun"]["config"]
            .as_object_mut()
            .unwrap()
            .remove("candidatePoolSize");
        json["config"]
            .as_object_mut()
            .unwrap()
            .remove("regionalCandidateLimit");
        json["lastRun"]["config"]
            .as_object_mut()
            .unwrap()
            .remove("regionalCandidateLimit");
        for result in json["lastRun"]["results"].as_array_mut().unwrap() {
            result.as_object_mut().unwrap().remove("measurementPassed");
        }
        json["config"].as_object_mut().unwrap().remove("maxTcpMs");
        json["lastRun"]["config"]
            .as_object_mut()
            .unwrap()
            .remove("maxTcpMs");
        json["config"]
            .as_object_mut()
            .unwrap()
            .remove("stabilityAttempts");
        json["lastRun"]["config"]
            .as_object_mut()
            .unwrap()
            .remove("stabilityAttempts");
        json["config"]
            .as_object_mut()
            .unwrap()
            .remove("stabilityLatencyMetric");
        json["lastRun"]["config"]
            .as_object_mut()
            .unwrap()
            .remove("stabilityLatencyMetric");
        json["config"]
            .as_object_mut()
            .unwrap()
            .remove("tcpLatencyScreening");
        json["lastRun"]["config"]
            .as_object_mut()
            .unwrap()
            .remove("tcpLatencyScreening");
        json["config"]
            .as_object_mut()
            .unwrap()
            .remove("stabilityFirst");
        json["lastRun"]["config"]
            .as_object_mut()
            .unwrap()
            .remove("stabilityFirst");
        let bytes = serde_json::to_vec(&json).unwrap();
        fs::write(path, &bytes).unwrap();
        bytes
    }

    #[test]
    fn replaces_existing_state_without_leaving_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut saved = SavedState::default();
        save(&path, &saved).unwrap();
        saved.config.regions = vec!["JP".into()];
        save(&path, &saved).unwrap();
        assert_eq!(load(&path).unwrap().config.regions, ["JP"]);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn invalid_state_reports_an_error_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, "broken").unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn old_state_gets_default_folders_but_an_explicitly_empty_library_stays_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("favorites-migration.json");
        let mut json = serde_json::to_value(SavedState::default()).unwrap();
        json.as_object_mut().unwrap().remove("favorites");
        let original = serde_json::to_vec(&json).unwrap();
        fs::write(&path, &original).unwrap();
        let migrated = load(&path).unwrap();
        assert_eq!(
            migrated
                .favorites
                .folders
                .iter()
                .map(|folder| folder.name.as_str())
                .collect::<Vec<_>>(),
            ["联通流量", "联通宽带", "移动流量"]
        );
        assert!(migrated.favorites.entries.is_empty());
        assert_eq!(fs::read(&path).unwrap(), original);
        let mut empty = migrated;
        empty.favorites.folders.clear();
        save(&path, &empty).unwrap();
        assert!(load(&path).unwrap().favorites.folders.is_empty());
    }

    #[test]
    fn legacy_library_revision_defaults_to_zero_and_new_revision_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("revision-compat.json");
        let mut json = serde_json::to_value(SavedState::default()).unwrap();
        json["favorites"]
            .as_object_mut()
            .unwrap()
            .remove("revision");
        fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        let mut loaded = load(&path).unwrap();
        assert_eq!(loaded.favorites.revision, 0);
        loaded.favorites.revision = 23;
        save(&path, &loaded).unwrap();
        assert_eq!(load(&path).unwrap().favorites.revision, 23);
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted["favorites"]["revision"], 23);
    }

    #[test]
    fn failed_interruption_recovery_preserves_library_and_revision() {
        let dir = tempfile::tempdir().unwrap();
        let blocking = dir.path().join("blocking");
        fs::write(&blocking, "occupied").unwrap();
        let mut saved = SavedState::default();
        saved.favorites.revision = 17;
        let folder = saved.favorites.folders[0].id.clone();
        let mut pending = legacy_run();
        pending.id = "interrupted-manual".into();
        pending.status = "running".into();
        let (mut job, _) = saved
            .favorites
            .prepare_test(
                pending,
                Some(&folder),
                None,
                Some("104.16.1.1"),
                Some("保留备注"),
            )
            .unwrap();
        job.run.results[0].tcp_ms = 51.0;
        saved.favorite_test = Some(job);
        let original = serde_json::to_value(&saved.favorites).unwrap();
        recover_interrupted_favorite_test(&mut saved, &blocking.join("state.json"));
        assert_eq!(serde_json::to_value(&saved.favorites).unwrap(), original);
        assert_eq!(saved.favorites.revision, 17);
        assert!(saved.warning.unwrap().contains("原收藏数据已保留"));
        let job = saved.favorite_test.unwrap();
        assert_eq!(job.run.status, "cancelled");
        assert_eq!(job.run.results[0].tcp_ms, 51.0);
    }

    #[test]
    fn favorites_reload_exact_snapshot_without_reapplying_current_scan_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("favorites-history.json");
        let mut saved = SavedState::default();
        let folder = saved.favorites.folders[0].id.clone();
        let mut old_run = legacy_run();
        old_run.config.max_tcp_ms = None;
        old_run.config.stability_attempts = None;
        old_run.config.regional_candidate_limit = None;
        old_run.config.stability_first = false;
        old_run.config.tcp_latency_screening = true;
        // Keep the saved result exactly as seen, even when newer policies would
        // classify this record differently during last-run migration.
        old_run.results[1].measurement_passed = true;
        old_run.results[1].tcp_ms = 350.0;
        let ip = old_run.results[1].ip.clone();
        saved
            .favorites
            .save_snapshot(&folder, &old_run, &ip, "历史快照")
            .unwrap();
        let original = serde_json::to_value(&saved.favorites).unwrap();
        let mut next_run = legacy_run();
        next_run.id = "next-run".into();
        next_run.config.max_tcp_ms = Some(100.0);
        next_run.results.clear();
        saved.last_run = Some(next_run);
        save(&path, &saved).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.last_run.as_ref().unwrap().id, "next-run");
        assert_eq!(serde_json::to_value(loaded.favorites).unwrap(), original);
    }

    #[test]
    fn interrupted_favorite_job_recovers_partial_evidence_without_changing_main_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("favorite-restart.json");
        let mut saved = SavedState::default();
        let folder = saved.favorites.folders[0].id.clone();
        let previous = legacy_run();
        for result in &previous.results[..2] {
            saved
                .favorites
                .save_snapshot(&folder, &previous, &result.ip, "keep note")
                .unwrap();
        }
        let unattempted = serde_json::to_value(&saved.favorites.entries[1]).unwrap();
        let mut pending = previous.clone();
        pending.id = "favorite-job".into();
        pending.status = "running".into();
        pending.finished_at = None;
        let (mut job, _) = saved
            .favorites
            .prepare_test(pending, Some(&folder), None, None, None)
            .unwrap();
        job.run.results[0].tcp_ms = 42.0;
        job.run.results[0].region_code = "GB".into();
        saved.favorite_test = Some(job);
        saved.last_run = Some(previous);
        saved.favorites.revision = 13;
        save(&path, &saved).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.favorites.revision, 14);
        // Recovery is persisted as one terminal snapshot, not regenerated on
        // every read with the same version and a different finishedAt value.
        let recovered = serde_json::to_value(&loaded.favorites).unwrap();
        assert_eq!(
            serde_json::to_value(load(&path).unwrap().favorites).unwrap(),
            recovered
        );
        let job = loaded.favorite_test.unwrap();
        assert_eq!(job.run.status, "cancelled");
        assert!(job.run.config.regions.iter().any(|region| region == "GB"));
        assert_eq!(loaded.favorites.entries[0].result.tcp_ms, 42.0);
        assert_eq!(loaded.favorites.entries[0].run_status, "cancelled");
        assert!(loaded.favorites.entries[0].targeted);
        assert_eq!(loaded.favorites.entries[0].note, "keep note");
        assert_eq!(
            serde_json::to_value(&loaded.favorites.entries[1]).unwrap(),
            unattempted
        );
        assert_eq!(loaded.last_run.unwrap().id, "legacy-run");
    }

    #[test]
    fn upgrades_legacy_builtin_coverage_without_rewriting_history_or_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-coverage.json");
        let mut run = legacy_run();
        run.config.max_candidates = 1024;
        let mut state = serde_json::to_value(SavedState {
            last_run: Some(run),
            ..Default::default()
        })
        .unwrap();
        state
            .as_object_mut()
            .unwrap()
            .remove("candidateCoverageVersion");
        state["config"]["maxCandidates"] = 1024.into();
        let original = serde_json::to_vec(&state).unwrap();
        fs::write(&path, &original).unwrap();

        let restored = load(&path).unwrap();
        assert_eq!(restored.config.max_candidates, CLOUDFLARE_CANDIDATE_COUNT);
        assert_eq!(
            restored.candidate_coverage_version,
            CANDIDATE_COVERAGE_VERSION
        );
        assert_eq!(restored.last_run.unwrap().config.max_candidates, 1024);
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn coverage_upgrade_preserves_custom_sources_and_other_explicit_caps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom-coverage.json");
        for (source, cap) in [("custom", 1024), ("cloudflare", 2048)] {
            let mut state = SavedState::default();
            state.candidate_coverage_version = 0;
            state.config.source = source.into();
            state.config.max_candidates = cap;
            save(&path, &state).unwrap();
            let restored = load(&path).unwrap();
            assert_eq!(restored.config.max_candidates, cap);
            assert_eq!(restored.config.source, source);
        }
    }

    #[test]
    fn manual_sparse_cap_after_coverage_upgrade_survives_reloading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manual-coverage.json");
        let mut state = SavedState::default();
        state.config.max_candidates = 1024;
        save(&path, &state).unwrap();
        assert_eq!(load(&path).unwrap().config.max_candidates, 1024);
    }

    #[test]
    fn legacy_precision_caps_remain_historical_while_next_scan_is_adaptive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-precision.json");
        let mut state = serde_json::to_value(SavedState {
            last_run: Some(legacy_run()),
            ..Default::default()
        })
        .unwrap();
        state["config"]
            .as_object_mut()
            .unwrap()
            .remove("precisionMode");
        state["lastRun"]["config"]
            .as_object_mut()
            .unwrap()
            .remove("precisionMode");
        state["config"]
            .as_object_mut()
            .unwrap()
            .remove("candidatePoolSize");
        state["lastRun"]["config"]
            .as_object_mut()
            .unwrap()
            .remove("candidatePoolSize");
        state["lastRun"]["config"]["candidatesPerRegion"] = 10.into();
        let original = serde_json::to_vec(&state).unwrap();
        fs::write(&path, &original).unwrap();

        let restored = load(&path).unwrap();
        assert_eq!(restored.config.precision_mode, "adaptive");
        assert_eq!(restored.config.candidate_pool_size, Some(20));
        let previous = restored.last_run.unwrap();
        assert_eq!(previous.config.precision_mode, "limited");
        assert_eq!(previous.config.candidate_pool_size, None);
        assert_eq!(previous.config.candidates_per_region, 10);
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn explicitly_saved_precision_mode_survives_reloading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("precision-mode.json");
        for mode in ["adaptive", "limited"] {
            let mut state = SavedState::default();
            state.config.precision_mode = mode.into();
            state.config.candidates_per_region = 47;
            save(&path, &state).unwrap();
            let restored = load(&path).unwrap();
            assert_eq!(restored.config.precision_mode, mode);
            assert_eq!(restored.config.candidates_per_region, 47);
        }
    }

    #[test]
    fn new_regional_candidate_cap_does_not_rewrite_historical_unlimited_scans() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-candidate-limit.json");
        let mut state = serde_json::to_value(SavedState {
            last_run: Some(legacy_run()),
            ..Default::default()
        })
        .unwrap();
        state["config"]
            .as_object_mut()
            .unwrap()
            .remove("regionalCandidateLimit");
        state["lastRun"]["config"]
            .as_object_mut()
            .unwrap()
            .remove("regionalCandidateLimit");
        state["lastRun"]["config"]["precisionMode"] = "adaptive".into();
        let original = serde_json::to_vec(&state).unwrap();
        fs::write(&path, &original).unwrap();

        let restored = load(&path).unwrap();
        assert_eq!(restored.config.regional_candidate_limit, Some(20));
        let previous = restored.last_run.unwrap();
        assert_eq!(previous.config.precision_mode, "adaptive");
        assert_eq!(previous.config.regional_candidate_limit, None);
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn new_two_failure_policy_is_enabled_only_for_active_settings_and_recorded_new_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stability-first.json");
        for recorded in [None, Some(false), Some(true)] {
            let mut run = legacy_run();
            run.config.max_stability_failure_percent = 100.0;
            let mut probes = vec![Some(20.0); 98];
            probes.extend([None, None]);
            run.results[1].tcp_stability = Some(crate::stability::summarize(100, 200.0, probes));
            run.results[1].measurement_passed = true;
            let mut state = serde_json::to_value(SavedState {
                last_run: Some(run),
                ..Default::default()
            })
            .unwrap();
            state["config"]["stabilityFirst"] = false.into();
            if let Some(recorded) = recorded {
                state["lastRun"]["config"]["stabilityFirst"] = recorded.into();
            } else {
                state["lastRun"]["config"]
                    .as_object_mut()
                    .unwrap()
                    .remove("stabilityFirst");
            }
            let original = serde_json::to_vec(&state).unwrap();
            fs::write(&path, &original).unwrap();

            let restored = load(&path).unwrap();
            assert!(restored.config.stability_first);
            restored.config.validate().unwrap();
            let run = restored.last_run.unwrap();
            assert_eq!(run.config.stability_first, recorded.unwrap_or(false));
            assert_eq!(run.results[1].eligible, !recorded.unwrap_or(false));
            assert_eq!(
                run.results[1].measurement_passed,
                !recorded.unwrap_or(false)
            );
            assert_eq!(fs::read(&path).unwrap(), original);
        }
    }

    #[test]
    fn explicit_candidate_cap_survives_and_null_active_cap_uses_twenty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("candidate-limit.json");
        let mut state = SavedState::default();
        state.config.regional_candidate_limit = Some(37);
        save(&path, &state).unwrap();
        assert_eq!(
            load(&path).unwrap().config.regional_candidate_limit,
            Some(37)
        );
        state.config.regional_candidate_limit = None;
        save(&path, &state).unwrap();
        assert_eq!(
            load(&path).unwrap().config.regional_candidate_limit,
            Some(20)
        );
    }

    #[test]
    fn old_default_candidate_cap_is_reduced_once_without_rewriting_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old-default-limit.json");
        let mut state = SavedState {
            candidate_coverage_version: 1,
            last_run: Some(legacy_run()),
            ..Default::default()
        };
        state.config.regional_candidate_limit = Some(50);
        state
            .last_run
            .as_mut()
            .unwrap()
            .config
            .regional_candidate_limit = Some(50);
        // A manual sampling cap introduced after the prior migration survives.
        state.config.max_candidates = 1024;
        save(&path, &state).unwrap();
        let mut loaded = load(&path).unwrap();
        assert_eq!(loaded.config.regional_candidate_limit, Some(20));
        assert_eq!(loaded.config.max_candidates, 1024);
        assert_eq!(
            loaded
                .last_run
                .as_ref()
                .unwrap()
                .config
                .regional_candidate_limit,
            Some(50)
        );
        loaded.config.regional_candidate_limit = Some(50);
        save(&path, &loaded).unwrap();
        assert_eq!(
            load(&path).unwrap().config.regional_candidate_limit,
            Some(50)
        );
    }

    #[test]
    fn disabled_budget_persists_in_active_settings_and_historical_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unlimited-budget.json");
        let mut state = SavedState {
            last_run: Some(legacy_run()),
            ..Default::default()
        };
        state.config.download_budget_enabled = false;
        state.config.download_budget_mi_b = 0;
        state
            .last_run
            .as_mut()
            .unwrap()
            .config
            .download_budget_enabled = false;
        save(&path, &state).unwrap();
        let restored = load(&path).unwrap();
        assert!(!restored.config.download_budget_enabled);
        assert_eq!(restored.config.download_budget_mi_b, 0);
        assert!(!restored.last_run.unwrap().config.download_budget_enabled);
    }

    #[test]
    fn budget_choice_survives_restart_in_both_directions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budget-choice.json");
        assert!(!load(&path).unwrap().config.download_budget_enabled);
        let mut state = SavedState::default();
        for enabled in [true, false, true] {
            state.config.download_budget_enabled = enabled;
            save(&path, &state).unwrap();
            state = load(&path).unwrap();
            assert_eq!(state.config.download_budget_enabled, enabled);
        }
    }

    #[test]
    fn missing_historical_budget_switch_keeps_original_budget_without_changing_active_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("historical-budgets.json");
        let run = legacy_run();
        let mut state = SavedState {
            last_run: Some(run.clone()),
            ..Default::default()
        };
        let folder = state.favorites.folders[0].id.clone();
        for result in &run.results[..2] {
            state
                .favorites
                .save_snapshot(&folder, &run, &result.ip, "legacy budget")
                .unwrap();
        }
        state.favorite_test = Some(FavoriteTestJob {
            run,
            favorite_ids: state
                .favorites
                .entries
                .iter()
                .map(|entry| entry.id.clone())
                .collect(),
        });
        let mut json = serde_json::to_value(state).unwrap();
        for pointer in [
            "/config",
            "/lastRun/config",
            "/favoriteTest/run/config",
            "/favorites/entries/0/config",
        ] {
            json.pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove("downloadBudgetEnabled");
        }
        // This newer snapshot explicitly disabled the budget; preserve it.
        json["favorites"]["entries"][1]["config"]["downloadBudgetEnabled"] = false.into();
        let bytes = serde_json::to_vec(&json).unwrap();
        fs::write(&path, &bytes).unwrap();
        let loaded = load(&path).unwrap();
        assert!(!loaded.config.download_budget_enabled);
        assert!(loaded.last_run.unwrap().config.download_budget_enabled);
        assert!(
            loaded
                .favorite_test
                .unwrap()
                .run
                .config
                .download_budget_enabled
        );
        assert!(loaded.favorites.entries[0].config.download_budget_enabled);
        assert!(!loaded.favorites.entries[1].config.download_budget_enabled);
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn recovers_only_legacy_recommendations_and_explicit_speed_filtered_complete_measurements() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.json");
        let original = write_legacy(&path, legacy_run());
        let run = load(&path).unwrap().last_run.unwrap();
        let flags: Vec<_> = run
            .results
            .iter()
            .map(|result| result.measurement_passed)
            .collect();
        assert_eq!(flags, [true, true, false, false, false, false, false]);
        // Zero measured milliseconds are not a failure signal.
        assert_eq!(run.results[0].tcp_ms, 0.0);
        assert_eq!(run.results[0].https_ms, 0.0);
        assert!(!run.results[0].eligible);
        assert!(run.results[0].has_only_speed_target_exclusion());
        assert!(run.results[1].eligible);
        assert_eq!(run.results[1].role.as_deref(), Some("primary"));
        assert_eq!(
            fs::read(&path).unwrap(),
            original,
            "loading must not rewrite the legacy record"
        );
    }

    #[test]
    fn recovering_interrupted_legacy_run_keeps_verified_evidence_without_claiming_recommendations()
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interrupted.json");
        let mut run = legacy_run();
        run.status = "running".into();
        run.progress.stage = "downloading".into();
        write_legacy(&path, run);
        let restored = load(&path).unwrap().last_run.unwrap();
        assert_eq!(restored.status, "cancelled");
        assert!(restored.results[0].measurement_passed);
        assert!(!restored.results[0].eligible);
        assert!(restored
            .results
            .iter()
            .all(|result| result.role.as_deref() == Some("candidate")));
        assert!(!restored.results[3].measurement_passed);
    }

    #[test]
    fn a_saved_true_flag_never_overrides_hard_failures_or_incomplete_downloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("current.json");
        let mut run = legacy_run();
        for result in &mut run.results {
            result.measurement_passed = true;
        }
        save(
            &path,
            &SavedState {
                last_run: Some(run),
                ..Default::default()
            },
        )
        .unwrap();
        let restored = load(&path).unwrap().last_run.unwrap();
        assert!(!restored.results[2].measurement_passed);
        assert!(!restored.results[3].measurement_passed);
        assert!(!restored.results[4].measurement_passed);
        assert!(!restored.results[5].measurement_passed);
    }

    #[test]
    fn legacy_run_without_a_tcp_gate_keeps_its_original_unrestricted_measurements() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-tcp.json");
        let mut run = legacy_run();
        run.results[1].tcp_ms = 350.0;
        let original = write_legacy(&path, run);
        let saved = load(&path).unwrap();
        assert_eq!(saved.config.max_tcp_ms, Some(230.0));
        let run = saved.last_run.unwrap();
        assert_eq!(run.config.max_tcp_ms, None);
        assert_eq!(run.results[1].tcp_ms, 350.0);
        assert!(run.results[1].eligible && run.results[1].measurement_passed);
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn explicit_tcp_gate_is_preserved_and_blocks_high_tcp_on_restore() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limited.json");
        let mut run = legacy_run();
        run.config.tcp_latency_screening = true;
        run.results[1].tcp_ms = 200.001;
        run.results[1].tcp_stability = Some(crate::stability::summarize(
            100,
            200.0,
            vec![Some(180.0); 100],
        ));
        run.results[1].measurement_passed = true;
        save(
            &path,
            &SavedState {
                last_run: Some(run),
                ..Default::default()
            },
        )
        .unwrap();
        let run = load(&path).unwrap().last_run.unwrap();
        assert_eq!(run.config.max_tcp_ms, Some(200.0));
        assert!(!run.results[1].eligible && !run.results[1].measurement_passed);
        assert_eq!(run.results[1].role.as_deref(), Some("candidate"));
    }

    #[test]
    fn null_top_level_tcp_setting_is_normalized_for_the_next_scan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("null-setting.json");
        let mut saved = SavedState::default();
        saved.config.max_tcp_ms = None;
        save(&path, &saved).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.config.max_tcp_ms, Some(230.0));
        loaded.config.validate().unwrap();
    }

    #[test]
    fn legacy_stability_stays_untested_while_next_scan_defaults_to_one_hundred() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-stability.json");
        let original = write_legacy(&path, legacy_run());
        let saved = load(&path).unwrap();
        assert_eq!(saved.config.stability_attempts, Some(100));
        let run = saved.last_run.unwrap();
        assert_eq!(run.config.stability_attempts, None);
        assert!(run.results.iter().all(|row| row.tcp_stability.is_none()));
        assert!(run.results[1].eligible && run.results[1].measurement_passed);
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn new_stability_policy_survives_restore_and_rejects_missing_or_partial_probes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new-stability.json");
        let mut run = legacy_run();
        run.results[1].tcp_stability = Some(crate::stability::summarize(
            100,
            200.0,
            vec![Some(20.0); 100],
        ));
        run.results[1].measurement_passed = true;
        save(
            &path,
            &SavedState {
                last_run: Some(run.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let valid = load(&path).unwrap().last_run.unwrap();
        assert_eq!(valid.config.stability_attempts, Some(100));
        assert!(valid.results[1].measurement_passed && valid.results[1].eligible);
        for partial in [false, true] {
            run.results[1].tcp_stability =
                partial.then(|| crate::stability::summarize(100, 200.0, vec![Some(20.0); 99]));
            run.results[1].measurement_passed = true;
            run.results[1].eligible = true;
            save(
                &path,
                &SavedState {
                    last_run: Some(run.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
            let restored = load(&path).unwrap().last_run.unwrap();
            assert!(!restored.results[1].measurement_passed && !restored.results[1].eligible);
            assert_eq!(restored.results[1].role.as_deref(), Some("candidate"));
        }
    }

    #[test]
    fn null_active_stability_setting_is_repaired_without_rewriting_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("active-stability-null.json");
        let mut saved = SavedState::default();
        saved.config.stability_attempts = None;
        save(&path, &saved).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.config.stability_attempts, Some(100));
        loaded.config.validate().unwrap();
    }

    #[test]
    fn historical_missing_latency_policy_keeps_max_while_next_scan_defaults_to_p95() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-policy.json");
        let original = write_legacy(&path, legacy_run());
        let restored = load(&path).unwrap();
        assert_eq!(restored.config.stability_latency_metric, "p95");
        assert_eq!(
            restored.last_run.unwrap().config.stability_latency_metric,
            "max"
        );
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn explicit_new_latency_policy_is_preserved_on_restore() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p95-policy.json");
        let mut run = legacy_run();
        let mut probes = vec![Some(150.0); 99];
        probes.push(Some(1000.0));
        run.results[1].tcp_stability = Some(crate::stability::summarize(100, 200.0, probes));
        run.results[1].measurement_passed = true;
        save(
            &path,
            &SavedState {
                last_run: Some(run),
                ..Default::default()
            },
        )
        .unwrap();
        let restored = load(&path).unwrap().last_run.unwrap();
        assert_eq!(restored.config.stability_latency_metric, "p95");
        assert!(restored.results[1].measurement_passed && restored.results[1].eligible);
        assert_eq!(
            restored.results[1].tcp_stability.as_ref().unwrap().max_ms,
            Some(1000.0)
        );
    }

    #[test]
    fn screening_policy_migration_preserves_history_and_turns_off_the_active_single_probe_gate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("screening-history.json");
        let original = write_legacy(&path, legacy_run());
        let saved = load(&path).unwrap();
        assert!(!saved.config.tcp_latency_screening);
        assert!(saved.last_run.unwrap().config.tcp_latency_screening);
        assert_eq!(fs::read(&path).unwrap(), original);
        let mut active = SavedState::default();
        active.config.tcp_latency_screening = true;
        save(&path, &active).unwrap();
        let saved = load(&path).unwrap();
        assert!(!saved.config.tcp_latency_screening);
        saved.config.validate().unwrap();
    }

    #[test]
    fn new_policy_restore_accepts_initial_350_when_full_p95_and_failure_rate_pass() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("screening-off.json");
        let mut run = legacy_run();
        run.results[1].tcp_ms = 350.0;
        let mut probes = vec![Some(180.0); 99];
        probes.push(None);
        run.results[1].tcp_stability = Some(crate::stability::summarize(100, 200.0, probes));
        run.results[1].measurement_passed = true;
        save(
            &path,
            &SavedState {
                last_run: Some(run),
                ..Default::default()
            },
        )
        .unwrap();
        let run = load(&path).unwrap().last_run.unwrap();
        assert!(!run.config.tcp_latency_screening);
        assert!(run.results[1].measurement_passed && run.results[1].eligible);
        assert_eq!(run.results[1].tcp_ms, 350.0);
    }

    #[test]
    fn historical_single_probe_rejection_is_not_revived_by_new_active_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("historical-rejected.json");
        let mut run = legacy_run();
        run.config.tcp_latency_screening = true;
        run.results[1].tcp_ms = 350.0;
        run.results[1].exclusion_reason = Some("TCP 延迟 350 ms，超过上限 200 ms".into());
        run.results[1].tcp_stability = Some(crate::stability::summarize(
            100,
            200.0,
            vec![Some(180.0); 100],
        ));
        run.results[1].measurement_passed = true;
        save(
            &path,
            &SavedState {
                last_run: Some(run),
                ..Default::default()
            },
        )
        .unwrap();
        let saved = load(&path).unwrap();
        assert!(!saved.config.tcp_latency_screening);
        let run = saved.last_run.unwrap();
        assert!(run.config.tcp_latency_screening);
        assert!(!run.results[1].measurement_passed && !run.results[1].eligible);
        assert!(run.results[1]
            .exclusion_reason
            .as_deref()
            .unwrap()
            .starts_with("TCP 延迟 350"));
    }
}
