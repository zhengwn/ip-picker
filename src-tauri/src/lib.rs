mod checkpoint;
#[cfg(feature = "desktop-smoke")]
mod desktop_smoke;
pub mod engine;
mod favorites;
mod history;
mod instance;
#[cfg(target_os = "macos")]
mod macos_menu;
pub mod model;
pub mod network;
pub mod stability;
mod storage;

use crate::{
    engine::{
        prepare_ip_retest, prepare_scan, run_download_test, run_targeted_scan, EngineOutcome,
        EngineUpdate, PreparedScan, UpdateCallback,
    },
    favorites::{checked_ipv4, FavoriteLibrary, FavoriteTestJob, FavoriteTestStart},
    history::{HistoryKind, HistoryMatch, HistorySummary, ScanHistory, HISTORY_LIMIT},
    model::{ScanCommandError, ScanConfig, ScanProgress, ScanRun},
    storage::SavedState,
};
#[cfg(feature = "desktop-smoke")]
use desktop_smoke::run_prepared_scan;
#[cfg(not(feature = "desktop-smoke"))]
use engine::run_prepared_scan;
use serde::Serialize;
use std::{
    net::Ipv4Addr,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tauri::{Emitter, Manager, State};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Keep slow UI serialization out of the network task. The single watch slot
/// retains only the newest snapshot while an earlier snapshot is being sent.
fn progress_bridge<F>(deliver: F) -> (UpdateCallback, tokio::task::JoinHandle<()>)
where
    F: Fn(EngineUpdate, bool) + Send + Sync + 'static,
{
    let (sender, mut receiver) = tokio::sync::watch::channel::<Option<EngineUpdate>>(None);
    let callback: UpdateCallback = Arc::new(move |update| {
        sender.send_replace(Some(update));
    });
    let deliver = Arc::new(deliver);
    let forwarder = tokio::spawn(async move {
        let mut last_results = None;
        while receiver.changed().await.is_ok() {
            let update = receiver.borrow_and_update().clone();
            if let Some(update) = update {
                // If a full snapshot was coalesced away, its next byte update
                // still carries those rows and must deliver them.
                let rows_changed = !last_results
                    .as_ref()
                    .is_some_and(|last| Arc::ptr_eq(last, &update.results));
                last_results = Some(update.results.clone());
                let deliver = deliver.clone();
                if tokio::task::spawn_blocking(move || deliver(update, rows_changed))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    });
    (callback, forwarder)
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProgressUpdate {
    id: String,
    downloaded_bytes: u64,
}

#[derive(Clone, Serialize)]
#[serde(untagged)]
enum DesktopUpdate<T> {
    Snapshot(T),
    Progress(ProgressUpdate),
}

fn apply_engine_update(
    run: &mut ScanRun,
    update: EngineUpdate,
    rows_changed: bool,
) -> Option<ProgressUpdate> {
    run.progress = update.progress;
    if rows_changed {
        run.results.clone_from(&update.results);
        if run.retest_ip.is_some() {
            run.record_observed_regions();
        }
        None
    } else {
        Some(ProgressUpdate {
            id: run.id.clone(),
            downloaded_bytes: run.progress.downloaded_bytes,
        })
    }
}

struct RuntimeState {
    saved: SavedState,
    cancellation: Option<CancellationToken>,
    worker: Option<watch::Receiver<()>>,
}

impl RuntimeState {
    fn begin_scan(
        &mut self,
        mut config: ScanConfig,
        token: CancellationToken,
        path: &std::path::Path,
        persist_config: bool,
        retest_ip: Option<&str>,
    ) -> Result<(ScanRun, PreparedScan), ScanCommandError> {
        if self.has_active_measurement() {
            return Err("已有测试正在运行，请先停止并等待结束".into());
        }
        // Parse every custom entry and sample once before replacing the latest run.
        let target = retest_ip.map(checked_ipv4).transpose()?;
        let prepared = if let Some(ip) = target {
            config.source = "custom".into();
            config.custom_ips = ip.to_string();
            config.max_candidates = 1;
            prepare_ip_retest(config.clone(), ip)?
        } else {
            prepare_scan(config.clone())?
        };
        let run = ScanRun {
            quick_validation: Some(!persist_config && target.is_none()),
            retest_ip: target.map(|ip| ip.to_string()),
            download_only: false,
            id: uuid::Uuid::new_v4().to_string(),
            status: "running".into(),
            started_at: chrono::Utc::now().to_rfc3339(),
            finished_at: None,
            config,
            progress: ScanProgress::default(),
            results: Vec::new(),
            error: None,
        };
        self.begin(run.clone(), token, path, persist_config && target.is_none())?;
        Ok((run, prepared))
    }

    fn has_active_measurement(&self) -> bool {
        self.cancellation.is_some()
            || self
                .saved
                .last_run
                .as_ref()
                .is_some_and(|run| run.status == "running")
            || self
                .saved
                .favorite_test
                .as_ref()
                .is_some_and(|job| job.run.status == "running")
    }

    /// Assign the next revision from the authoritative state under the caller's
    /// mutex. A failed write never exposes the candidate library or consumes a
    /// revision; command responses and events therefore share one version order.
    fn commit_favorites(
        &mut self,
        mut next: SavedState,
        path: &std::path::Path,
    ) -> Result<FavoriteLibrary, String> {
        next.favorites.revision = self.saved.favorites.next_revision()?;
        next.warning = None;
        storage::save_replacing_history(path, &next, &self.saved.scan_history)?;
        self.saved = next;
        Ok(self.saved.favorites.clone())
    }

    fn update_favorites<F>(
        &mut self,
        path: &std::path::Path,
        change: F,
    ) -> Result<FavoriteLibrary, String>
    where
        F: FnOnce(&SavedState, &mut FavoriteLibrary) -> Result<(), String>,
    {
        // Work from the latest state while holding the caller's mutex. A scan
        // update cannot be overwritten by a separately loaded saved snapshot.
        let mut next = self.saved.clone();
        next.apply_pending_favorite_results();
        change(&self.saved, &mut next.favorites)?;
        self.commit_favorites(next, path)
    }

    fn begin(
        &mut self,
        run: ScanRun,
        token: CancellationToken,
        path: &std::path::Path,
        persist_config: bool,
    ) -> Result<(), String> {
        if self.has_active_measurement() {
            return Err("已有测试正在运行，请先停止并等待结束".into());
        }
        let old_saved = self.saved.clone();
        let old_token = self.cancellation.clone();
        if persist_config {
            self.saved.config = run.config.clone();
        }
        if let Some(previous) = self.saved.last_run.replace(run) {
            self.saved.archive_run(HistoryKind::Scan, previous);
        }
        self.saved.warning = None;
        self.saved.measurement_revision = 0;
        self.cancellation = Some(token);
        if let Err(error) =
            storage::save_replacing_history(path, &self.saved, &old_saved.scan_history)
        {
            self.saved = old_saved;
            self.cancellation = old_token;
            return Err(error);
        }
        Ok(())
    }

    fn begin_favorite_test(
        &mut self,
        config: ScanConfig,
        download_only: bool,
        folder_id: Option<&str>,
        favorite_id: Option<&str>,
        ip: Option<&str>,
        note: Option<&str>,
        token: CancellationToken,
        path: &std::path::Path,
    ) -> Result<(FavoriteTestStart, Vec<Ipv4Addr>), String> {
        if self.has_active_measurement() {
            return Err("已有测试正在运行，请先停止并等待结束".into());
        }
        let mut next = self.saved.clone();
        next.apply_pending_favorite_results();
        // Capture pending user settings before prepare_test replaces the source
        // with the explicit targets. A targeted test can legitimately ignore an
        // empty custom source, but that invalid source must not become active.
        if config.validate().is_ok() {
            next.config = config.clone();
        }
        let (job, ips) = next.favorites.prepare_test(
            ScanRun {
                quick_validation: Some(false),
                retest_ip: None,
                download_only,
                id: uuid::Uuid::new_v4().to_string(),
                status: "running".into(),
                started_at: chrono::Utc::now().to_rfc3339(),
                finished_at: None,
                config,
                progress: ScanProgress::default(),
                results: Vec::new(),
                error: None,
            },
            folder_id,
            favorite_id,
            ip,
            note,
        )?;
        if let Some(previous) = next.favorite_test.replace(job.clone()) {
            next.archive_run(HistoryKind::Favorite, previous.run);
        }
        next.measurement_revision = 0;
        let favorites = self.commit_favorites(next, path)?;
        self.cancellation = Some(token);
        Ok((FavoriteTestStart { favorites, job }, ips))
    }

    fn complete_scan(
        &mut self,
        run_id: &str,
        outcome: Result<EngineOutcome, String>,
        cancelled: bool,
        path: &std::path::Path,
    ) -> Option<(ScanRun, Option<String>)> {
        let run = self.saved.last_run.as_mut()?;
        if run.id != run_id || run.status != "running" {
            return None;
        }
        finish_run(run, outcome, cancelled);
        let snapshot = run.clone();
        self.cancellation = None;
        let previous_history = self.saved.scan_history.clone();
        self.saved.retain_recent_history();
        let warning = match storage::save_replacing_history(path, &self.saved, &previous_history) {
            Ok(()) => {
                storage::clear_checkpoint(path);
                None
            }
            Err(error) => {
                self.saved.scan_history = previous_history;
                self.saved.warning = Some(error.clone());
                Some(format!(
                    "本轮结果尚未保存到本地，可先复制需要的 IP：{error}"
                ))
            }
        };
        Some((snapshot, warning))
    }

    /// Commit each row and the terminal job in one atomic saved-state write.
    /// A write failure retains the last saved measurements in the library;
    /// the completed job remains available in memory for inspection/retry.
    fn complete_favorite_test(
        &mut self,
        run_id: &str,
        outcome: Result<EngineOutcome, String>,
        cancelled: bool,
        path: &std::path::Path,
    ) -> Option<(FavoriteTestStart, Option<String>)> {
        let job = self.saved.favorite_test.as_mut()?;
        if job.run.id != run_id || job.run.status != "running" {
            return None;
        }
        finish_run(&mut job.run, outcome, cancelled);
        job.record_observed_regions();
        self.cancellation = None;
        self.saved.pending_favorite_results = true;
        let mut next = self.saved.clone();
        next.apply_pending_favorite_results();
        next.retain_recent_history();
        let warning = match self.commit_favorites(next, path) {
            Ok(_) => {
                storage::clear_checkpoint(path);
                None
            }
            Err(error) => {
                self.saved.warning = Some(error.clone());
                Some(format!(
                    "收藏测速结果尚未保存到本地，原收藏数据已保留：{error}"
                ))
            }
        };
        Some((
            FavoriteTestStart {
                favorites: self.saved.favorites.clone(),
                job: self.saved.favorite_test.as_ref().unwrap().clone(),
            },
            warning,
        ))
    }

    fn interrupt_for_shutdown(&mut self, path: &std::path::Path) -> Result<(), String> {
        if !self.has_active_measurement()
            && !self.saved.pending_favorite_results
            && self.saved.warning.is_none()
        {
            return Ok(());
        }
        if let Some(token) = &self.cancellation {
            token.cancel();
        }
        let mut next = self.saved.clone();
        if let Some(run) = next.last_run.as_mut() {
            if run.status == "running" {
                run.status = "cancelled".into();
                run.finished_at = Some(chrono::Utc::now().to_rfc3339());
                run.progress.stage = "cancelled".into();
                run.progress.stability_batch = None;
                run.progress.precision_batch = None;
                run.progress.message = "关闭应用时测试中断".into();
                for result in &mut run.results {
                    result.role = Some("candidate".into());
                    result.mark_cancelled(&run.config);
                }
            }
        }
        if let Some(job) = next
            .favorite_test
            .as_mut()
            .filter(|job| job.run.status == "running")
        {
            job.interrupt("关闭应用时收藏测速中断，已保留已完成数据");
            next.pending_favorite_results = true;
        }
        next.retain_recent_history();
        if next.pending_favorite_results {
            next.apply_pending_favorite_results();
            self.commit_favorites(next, path)?;
        } else {
            next.warning = None;
            storage::save_replacing_history(path, &next, &self.saved.scan_history)?;
            self.saved = next;
        }
        self.cancellation = None;
        storage::clear_checkpoint(path);
        Ok(())
    }
}

fn finish_run(run: &mut ScanRun, outcome: Result<EngineOutcome, String>, cancelled: bool) {
    match outcome {
        Ok(outcome) => {
            run.progress = outcome.progress;
            run.results = outcome.results;
            run.status = if cancelled || outcome.cancelled {
                "cancelled"
            } else {
                "completed"
            }
            .into();
        }
        Err(error) => {
            run.status = if cancelled { "cancelled" } else { "failed" }.into();
            run.error = Some(error);
        }
    }
    if run.retest_ip.is_some() {
        run.record_observed_regions();
    }
    run.progress.stage = run.status.clone();
    run.progress.stability_batch = None;
    run.progress.precision_batch = None;
    run.finished_at = Some(chrono::Utc::now().to_rfc3339());
    if run.status != "completed" {
        for result in &mut run.results {
            result.role = Some("candidate".into());
            if run.status == "cancelled" {
                result.mark_cancelled(&run.config);
            }
        }
        run.progress.message = if run.status == "cancelled" {
            "测试已停止，保留已完成数据"
        } else {
            "测试失败，请查看错误详情"
        }
        .into();
    }
}

#[derive(Clone)]
struct AppState {
    inner: Arc<Mutex<RuntimeState>>,
    storage_path: PathBuf,
    shutdown_gate: Arc<tokio::sync::Mutex<()>>,
}

impl AppState {
    fn start_checkpoint_writer(
        &self,
        kind: HistoryKind,
        run_id: String,
    ) -> (watch::Sender<u64>, tokio::task::JoinHandle<()>) {
        let state = self.clone();
        checkpoint::start(move |requested_revision| {
            let (run, revision) = {
                let inner = state.inner.lock().map_err(|_| "应用状态不可用")?;
                let run = match kind {
                    HistoryKind::Scan => inner.saved.last_run.as_ref(),
                    HistoryKind::Favorite => inner.saved.favorite_test.as_ref().map(|job| &job.run),
                };
                let Some(run) = run.filter(|run| run.id == run_id && run.status == "running")
                else {
                    return Ok(requested_revision);
                };
                (run.clone(), inner.saved.measurement_revision)
            };
            storage::save_checkpoint(&state.storage_path, &run, revision)?;
            Ok(revision)
        })
    }

    async fn prepare_shutdown(&self) -> Result<Option<FavoriteLibrary>, String> {
        let _gate = self.shutdown_gate.lock().await;
        let worker = {
            let inner = self.inner.lock().map_err(|_| "应用状态不可用")?;
            if let Some(token) = &inner.cancellation {
                token.cancel();
            }
            inner.worker.clone()
        };
        if let Some(mut worker) = worker {
            // The worker holds its sender through the final write and events.
            let _ = worker.changed().await;
        }

        let state = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut inner = state.inner.lock().map_err(|_| "应用状态不可用")?;
            let revision = inner.saved.favorites.revision;
            inner.interrupt_for_shutdown(&state.storage_path)?;
            Ok((inner.saved.favorites.revision != revision).then(|| inner.saved.favorites.clone()))
        })
        .await
        .map_err(|error| format!("等待测速保存失败：{error}"))?
    }
}

#[tauri::command]
async fn prepare_shutdown(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    if let Some(favorites) = state.prepare_shutdown().await? {
        let _ = app.emit("favorites-update", favorites);
    }
    Ok(())
}

#[tauri::command]
fn load_state(state: State<'_, AppState>) -> Result<SavedState, String> {
    let mut snapshot = state
        .inner
        .lock()
        .map_err(|_| "应用状态不可用")?
        .saved
        .clone();
    snapshot.scan_history.clear();
    snapshot.history_baselines.clear();
    Ok(snapshot)
}

#[tauri::command]
fn get_scan(state: State<'_, AppState>) -> Result<Option<ScanRun>, String> {
    Ok(state
        .inner
        .lock()
        .map_err(|_| "应用状态不可用")?
        .saved
        .last_run
        .clone())
}

#[tauri::command]
fn get_scan_history(state: State<'_, AppState>) -> Result<ScanHistory, String> {
    let inner = state.inner.lock().map_err(|_| "应用状态不可用")?;
    let entries = inner
        .saved
        .recent_history()
        .into_iter()
        .map(|(kind, run)| HistorySummary::new(kind, run))
        .collect();
    Ok(ScanHistory {
        limit: HISTORY_LIMIT,
        entries,
    })
}

#[tauri::command]
fn get_scan_history_run(
    run_id: String,
    state: State<'_, AppState>,
) -> Result<Option<ScanRun>, String> {
    let inner = state.inner.lock().map_err(|_| "应用状态不可用")?;
    Ok(inner
        .saved
        .recent_history()
        .into_iter()
        .find_map(|(_, run)| (run.id == run_id).then(|| run.clone())))
}

#[tauri::command]
fn compare_scan_history_ip(
    ip: String,
    state: State<'_, AppState>,
) -> Result<Vec<HistoryMatch>, String> {
    let ip = checked_ipv4(&ip)?.to_string();
    let inner = state.inner.lock().map_err(|_| "应用状态不可用")?;
    Ok(inner.saved.history_comparison(&ip))
}

#[tauri::command]
fn set_scan_history_baseline(
    ip: String,
    run_id: Option<String>,
    state: State<'_, AppState>,
) -> Result<Vec<HistoryMatch>, String> {
    let ip = checked_ipv4(&ip)?.to_string();
    let mut inner = state.inner.lock().map_err(|_| "应用状态不可用")?;
    let mut next = inner.saved.clone();
    next.set_history_baseline(&ip, run_id.as_deref())?;
    storage::save(&state.storage_path, &next)?;
    inner.saved = next;
    Ok(inner.saved.history_comparison(&ip))
}

#[tauri::command]
fn get_favorites(state: State<'_, AppState>) -> Result<FavoriteLibrary, String> {
    Ok(state
        .inner
        .lock()
        .map_err(|_| "应用状态不可用")?
        .saved
        .favorites
        .clone())
}

#[tauri::command]
fn get_favorite_test(state: State<'_, AppState>) -> Result<Option<FavoriteTestJob>, String> {
    Ok(state
        .inner
        .lock()
        .map_err(|_| "应用状态不可用")?
        .saved
        .favorite_test
        .clone())
}

#[tauri::command]
fn save_config(config: ScanConfig, state: State<'_, AppState>) -> Result<(), ScanCommandError> {
    config.validate_with_candidates()?;
    let mut inner = state.inner.lock().map_err(|_| "应用状态不可用")?;
    let old = inner.saved.clone();
    inner.saved.config = config;
    inner.saved.warning = None;
    if let Err(error) = storage::save(&state.storage_path, &inner.saved) {
        inner.saved = old;
        return Err(error.into());
    }
    Ok(())
}

#[tauri::command]
fn create_favorite_folder(
    name: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<FavoriteLibrary, String> {
    let favorites = state
        .inner
        .lock()
        .map_err(|_| "应用状态不可用")?
        .update_favorites(&state.storage_path, |_, favorites| {
            favorites.create_folder(&name)
        })?;
    let _ = app.emit("favorites-update", favorites.clone());
    Ok(favorites)
}

#[tauri::command]
fn rename_favorite_folder(
    folder_id: String,
    name: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<FavoriteLibrary, String> {
    let favorites = state
        .inner
        .lock()
        .map_err(|_| "应用状态不可用")?
        .update_favorites(&state.storage_path, |_, favorites| {
            favorites.rename_folder(&folder_id, &name)
        })?;
    let _ = app.emit("favorites-update", favorites.clone());
    Ok(favorites)
}

#[tauri::command]
fn delete_favorite_folder(
    folder_id: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<FavoriteLibrary, String> {
    let favorites = state
        .inner
        .lock()
        .map_err(|_| "应用状态不可用")?
        .update_favorites(&state.storage_path, |_, favorites| {
            favorites.delete_folder(&folder_id)
        })?;
    let _ = app.emit("favorites-update", favorites.clone());
    Ok(favorites)
}

#[tauri::command]
fn save_favorite(
    folder_id: String,
    run_id: String,
    ip: String,
    note: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<FavoriteLibrary, String> {
    let favorites = state
        .inner
        .lock()
        .map_err(|_| "应用状态不可用")?
        .update_favorites(&state.storage_path, |saved, favorites| {
            let run = saved.last_run.as_ref().ok_or("还没有测试结果")?;
            if run.id != run_id {
                return Err("测试结果已更新，请刷新后重新收藏".into());
            }
            favorites.save_snapshot(&folder_id, run, &ip, &note)
        })?;
    let _ = app.emit("favorites-update", favorites.clone());
    Ok(favorites)
}

#[tauri::command]
fn add_favorite_ip(
    folder_id: String,
    ip: String,
    note: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<FavoriteLibrary, String> {
    let favorites = state
        .inner
        .lock()
        .map_err(|_| "应用状态不可用")?
        .update_favorites(&state.storage_path, |saved, favorites| {
            favorites.add_manual_ip(&folder_id, &ip, &note, &saved.config)
        })?;
    let _ = app.emit("favorites-update", favorites.clone());
    Ok(favorites)
}

#[tauri::command]
fn update_favorite(
    favorite_id: String,
    folder_id: String,
    note: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<FavoriteLibrary, String> {
    let favorites = state
        .inner
        .lock()
        .map_err(|_| "应用状态不可用")?
        .update_favorites(&state.storage_path, |_, favorites| {
            favorites.update_entry(&favorite_id, &folder_id, &note)
        })?;
    let _ = app.emit("favorites-update", favorites.clone());
    Ok(favorites)
}

#[tauri::command]
fn delete_favorite(
    favorite_id: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<FavoriteLibrary, String> {
    let favorites = state
        .inner
        .lock()
        .map_err(|_| "应用状态不可用")?
        .update_favorites(&state.storage_path, |_, favorites| {
            favorites.delete_entry(&favorite_id)
        })?;
    let _ = app.emit("favorites-update", favorites.clone());
    Ok(favorites)
}

#[tauri::command]
async fn start_favorite_test(
    config: ScanConfig,
    download_only: Option<bool>,
    folder_id: Option<String>,
    favorite_id: Option<String>,
    ip: Option<String>,
    note: Option<String>,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<FavoriteTestStart, String> {
    let _gate = state.shutdown_gate.try_lock().map_err(|_| "窗口正在关闭")?;
    let cancellation = CancellationToken::new();
    let download_only = download_only.unwrap_or(false);
    let (done, worker) = watch::channel(());
    let (started, ips) = {
        let mut inner = state.inner.lock().map_err(|_| "应用状态不可用")?;
        let started = inner.begin_favorite_test(
            config,
            download_only,
            folder_id.as_deref(),
            favorite_id.as_deref(),
            ip.as_deref(),
            note.as_deref(),
            cancellation.clone(),
            &state.storage_path,
        )?;
        inner.worker = Some(worker);
        started
    };
    let _ = app.emit("favorites-update", started.favorites.clone());
    let _ = app.emit("favorite-test-update", started.job.clone());
    let owned_state = state.inner().clone();
    let run_id = started.job.run.id.clone();
    let config = started.job.run.config.clone();
    tauri::async_runtime::spawn(async move {
        let callback_state = owned_state.clone();
        let callback_app = app.clone();
        let callback_id = run_id.clone();
        let (checkpoint_updates, checkpoint_writer) =
            owned_state.start_checkpoint_writer(HistoryKind::Favorite, run_id.clone());
        let (callback, forwarder) = progress_bridge(move |update, rows_changed| {
            let snapshot = callback_state.inner.lock().ok().and_then(|mut inner| {
                let current = inner.saved.favorite_test.as_mut()?;
                if current.run.id != callback_id || current.run.status != "running" {
                    return None;
                }
                let progress = apply_engine_update(&mut current.run, update, rows_changed);
                let has_observations =
                    progress.is_some() || current.run.results.iter().any(favorites::was_attempted);
                let payload = if let Some(progress) = progress {
                    DesktopUpdate::Progress(progress)
                } else {
                    current.record_observed_regions();
                    DesktopUpdate::Snapshot(current.clone())
                };
                inner.saved.measurement_revision += 1;
                Some((payload, inner.saved.measurement_revision, has_observations))
            });
            if let Some((payload, revision, has_observations)) = snapshot {
                if has_observations {
                    checkpoint_updates.send_replace(revision);
                }
                let _ = callback_app.emit("favorite-test-update", payload);
            }
        });
        let outcome = if download_only {
            run_download_test(config, ips, cancellation.clone(), callback).await
        } else {
            run_targeted_scan(config, ips, cancellation.clone(), callback).await
        };
        let _ = forwarder.await;
        let _ = checkpoint_writer.await;
        let completed = owned_state.inner.lock().ok().and_then(|mut inner| {
            inner.complete_favorite_test(
                &run_id,
                outcome,
                cancellation.is_cancelled(),
                &owned_state.storage_path,
            )
        });
        if let Some((snapshot, warning)) = completed {
            if let Some(warning) = warning {
                let _ = app.emit("storage-warning", warning);
            }
            let _ = app.emit("favorites-update", snapshot.favorites);
            let _ = app.emit("favorite-test-update", snapshot.job);
        }
        drop(done);
    });
    Ok(started)
}

#[tauri::command]
fn cancel_favorite_test(run_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let inner = state.inner.lock().map_err(|_| "应用状态不可用")?;
    if inner
        .saved
        .favorite_test
        .as_ref()
        .is_some_and(|job| job.run.id == run_id && job.run.status == "running")
    {
        if let Some(token) = &inner.cancellation {
            token.cancel();
        }
    }
    Ok(())
}

#[tauri::command]
async fn start_scan(
    config: ScanConfig,
    persist_config: Option<bool>,
    retest_ip: Option<String>,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<ScanRun, ScanCommandError> {
    let _gate = state.shutdown_gate.try_lock().map_err(|_| "窗口正在关闭")?;
    let cancellation = CancellationToken::new();
    let (done, worker) = watch::channel(());
    let (run, prepared) = {
        let mut inner = state.inner.lock().map_err(|_| "应用状态不可用")?;
        let started = inner.begin_scan(
            config,
            cancellation.clone(),
            &state.storage_path,
            persist_config.unwrap_or(true),
            retest_ip.as_deref(),
        )?;
        inner.worker = Some(worker);
        started
    };
    let owned_state = state.inner().clone();
    let run_id = run.id.clone();
    tauri::async_runtime::spawn(async move {
        let callback_state = owned_state.clone();
        let callback_app = app.clone();
        let callback_id = run_id.clone();
        let (checkpoint_updates, checkpoint_writer) =
            owned_state.start_checkpoint_writer(HistoryKind::Scan, run_id.clone());
        let (callback, forwarder) = progress_bridge(move |update, rows_changed| {
            let snapshot = callback_state.inner.lock().ok().and_then(|mut inner| {
                let current = inner.saved.last_run.as_mut()?;
                if current.id != callback_id || current.status != "running" {
                    return None;
                }
                let progress = apply_engine_update(current, update, rows_changed);
                let has_observations =
                    progress.is_some() || current.results.iter().any(favorites::was_attempted);
                let payload = match progress {
                    Some(progress) => DesktopUpdate::Progress(progress),
                    None => DesktopUpdate::Snapshot(current.clone()),
                };
                inner.saved.measurement_revision += 1;
                Some((payload, inner.saved.measurement_revision, has_observations))
            });
            // Serialization can be expensive; never retain the state lock here.
            if let Some((payload, revision, has_observations)) = snapshot {
                if has_observations {
                    checkpoint_updates.send_replace(revision);
                }
                let _ = callback_app.emit("scan-update", payload);
            }
        });
        let outcome = run_prepared_scan(prepared, cancellation.clone(), callback).await;
        // The scan drops its callback sender. Drain the last queued snapshot
        // before publishing the authoritative completed/cancelled result.
        let _ = forwarder.await;
        // The progress forwarder owns the checkpoint sender. Join any write
        // already in flight before the authoritative terminal state is saved.
        let _ = checkpoint_writer.await;
        let completed = owned_state.inner.lock().ok().and_then(|mut inner| {
            inner.complete_scan(
                &run_id,
                outcome,
                cancellation.is_cancelled(),
                &owned_state.storage_path,
            )
        });
        if let Some((snapshot, warning)) = completed {
            if let Some(warning) = warning {
                let _ = app.emit("storage-warning", warning);
            }
            let _ = app.emit("scan-update", snapshot);
        }
        drop(done);
    });
    Ok(run)
}

#[tauri::command]
fn cancel_scan(run_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let inner = state.inner.lock().map_err(|_| "应用状态不可用")?;
    if inner
        .saved
        .last_run
        .as_ref()
        .is_some_and(|run| run.id == run_id && run.status == "running")
    {
        if let Some(token) = &inner.cancellation {
            token.cancel();
        }
    }
    Ok(())
}

#[tauri::command]
fn copy_text(text: String, app: tauri::AppHandle) -> Result<(), String> {
    if text.len() > 1_048_576 {
        return Err("复制内容过长".into());
    }
    app.clipboard()
        .write_text(text)
        .map_err(|e| format!("复制失败：{e}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default().plugin(tauri_plugin_clipboard_manager::init());
    #[cfg(target_os = "macos")]
    let builder = builder
        .menu(macos_menu::menu)
        .on_menu_event(macos_menu::on_menu_event);
    let context = tauri::generate_context!();
    #[cfg(feature = "desktop-smoke")]
    let (builder, context) = desktop_smoke::configure(builder, context);
    builder
        .setup(|app| {
            #[cfg(not(feature = "desktop-smoke"))]
            let data_dir = app.path().app_data_dir()?;
            #[cfg(feature = "desktop-smoke")]
            let data_dir = desktop_smoke::data_dir()?;
            // Acquire before reading or recovering state; a second process must
            // never even load a snapshot that it could later write back.
            let guard = instance::InstanceGuard::acquire(&data_dir)?;
            let path = data_dir.join("state.json");
            let saved = storage::load_for_startup(&path)?;
            app.manage(guard);
            app.manage(AppState {
                inner: Arc::new(Mutex::new(RuntimeState {
                    saved,
                    cancellation: None,
                    worker: None,
                })),
                storage_path: path,
                shutdown_gate: Arc::default(),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            load_state,
            prepare_shutdown,
            get_scan,
            get_scan_history,
            get_scan_history_run,
            compare_scan_history_ip,
            set_scan_history_baseline,
            get_favorites,
            get_favorite_test,
            save_config,
            start_scan,
            cancel_scan,
            start_favorite_test,
            cancel_favorite_test,
            create_favorite_folder,
            rename_favorite_folder,
            delete_favorite_folder,
            save_favorite,
            add_favorite_ip,
            update_favorite,
            delete_favorite,
            copy_text
        ])
        .run(context)
        .expect("IP Picker 启动失败");
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::history::compare_ip;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_progress_delivery_keeps_only_the_latest_snapshot_without_blocking_producers() {
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let received = delivered.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Mutex::new(Some(started_tx));
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let (callback, forwarder) = progress_bridge(move |update, rows_changed| {
            let number = update.progress.completed;
            received.lock().unwrap().push((number, rows_changed));
            if number == 1 {
                started_tx.lock().unwrap().take().unwrap().send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap();
            }
        });
        let update = |number, results| EngineUpdate {
            progress: ScanProgress {
                completed: number,
                ..Default::default()
            },
            results,
        };
        let first_rows = Arc::new(Vec::new());
        let latest_rows = Arc::new(Vec::new());
        callback(update(1, first_rows));
        started_rx.await.unwrap();
        for number in 2..=100 {
            callback(update(number, latest_rows.clone()));
        }
        drop(callback);
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), forwarder)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*delivered.lock().unwrap(), vec![(1, true), (100, true)]);
    }

    fn run(id: &str) -> ScanRun {
        ScanRun {
            quick_validation: Some(false),
            retest_ip: None,
            download_only: false,
            id: id.into(),
            status: "completed".into(),
            started_at: "2026-10-23T00:00:00Z".into(),
            finished_at: Some("2026-10-23T00:01:00Z".into()),
            config: ScanConfig::default(),
            progress: ScanProgress::default(),
            results: Vec::new(),
            error: None,
        }
    }

    #[tokio::test]
    async fn shutdown_waits_for_final_observations_and_can_retry_a_failed_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let token = CancellationToken::new();
        let (done, worker) = watch::channel(());
        let mut inner = RuntimeState {
            saved: SavedState::default(),
            cancellation: None,
            worker: Some(worker),
        };
        let mut active = run("closing-scan");
        active.status = "running".into();
        inner.begin(active, token.clone(), &path, true).unwrap();
        let original = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let state = AppState {
            inner: Arc::new(Mutex::new(inner)),
            storage_path: path.clone(),
            shutdown_gate: Arc::default(),
        };

        let close_state = state.clone();
        let closing = tokio::spawn(async move { close_state.prepare_shutdown().await });
        token.cancelled().await;
        assert!(!closing.is_finished());
        let mut row = model::IpResult::new(Ipv4Addr::LOCALHOST);
        row.tcp_ms = 42.0;
        let (snapshot, warning) = state
            .inner
            .lock()
            .unwrap()
            .complete_scan(
                "closing-scan",
                Ok(EngineOutcome {
                    progress: ScanProgress::default(),
                    results: vec![row],
                    cancelled: true,
                }),
                true,
                &path,
            )
            .unwrap();
        assert_eq!(snapshot.status, "cancelled");
        assert!(warning.is_some());
        drop(done);
        assert!(closing.await.unwrap().is_err());

        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, original).unwrap();
        state.prepare_shutdown().await.unwrap();
        let durable = storage::load(&path).unwrap().last_run.unwrap();
        assert_eq!(durable.status, "cancelled");
        assert_eq!(durable.results[0].tcp_ms, 42.0);
    }

    #[test]
    fn quick_scan_persists_the_run_with_its_own_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut inner = RuntimeState {
            saved: SavedState::default(),
            cancellation: None,
            worker: None,
        };
        let config = ScanConfig {
            max_candidates: 100,
            ..ScanConfig::default()
        };
        let (started, _) = inner
            .begin_scan(config, CancellationToken::new(), &path, false, None)
            .unwrap();
        let saved = storage::load(&path).unwrap();
        assert_eq!(
            saved.config.max_candidates,
            ScanConfig::default().max_candidates
        );
        let persisted = saved.last_run.unwrap();
        assert_eq!(persisted.id, started.id);
        assert_eq!(persisted.quick_validation, Some(true));
        assert_eq!(persisted.config.max_candidates, 100);
    }

    #[test]
    fn history_survives_retesting_the_same_ip_and_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let ip = "104.16.1.1";
        let mut row = model::IpResult::new(ip.parse().unwrap());
        row.tcp_ms = 22.0;
        let mut older = run("existing-scan");
        older.started_at = "2000-01-01T00:00:00Z".into();
        older.finished_at = Some("2000-01-01T00:01:00Z".into());
        older.results.push(row.clone());
        let saved = SavedState {
            last_run: Some(older.clone()),
            ..Default::default()
        };
        storage::save(&path, &saved).unwrap();
        let mut inner = RuntimeState {
            saved: storage::load(&path).unwrap(),
            cancellation: None,
            worker: None,
        };
        assert_eq!(inner.saved.recent_history()[0].1.id, older.id);
        assert!(inner.saved.scan_history.is_empty());

        let folder = inner.saved.favorites.folders[0].id.clone();
        for (interface, kind, tcp_ms) in [
            ("en7", network::InterfaceKind::Wired, 40.0),
            ("en0", network::InterfaceKind::Wifi, 200.0),
        ] {
            let (started, _) = inner
                .begin_favorite_test(
                    ScanConfig::default(),
                    false,
                    Some(&folder),
                    None,
                    Some(ip),
                    None,
                    CancellationToken::new(),
                    &path,
                )
                .unwrap();
            row.tcp_ms = tcp_ms;
            let outcome = EngineOutcome {
                progress: ScanProgress {
                    network_path: Some(network::NetworkPath {
                        sample_ip: ip.into(),
                        interface_name: interface.into(),
                        tunnel_interface: false,
                        interface_kind: Some(kind),
                        captured_at: started.job.run.started_at,
                    }),
                    ..Default::default()
                },
                results: vec![row.clone()],
                cancelled: false,
            };
            let (_, warning) = inner
                .complete_favorite_test(&started.job.run.id, Ok(outcome), false, &path)
                .unwrap();
            assert!(warning.is_none());
        }
        let restored = storage::load(&path).unwrap();
        assert_eq!(restored.scan_history.len(), 1);
        let matches = compare_ip(restored.recent_history(), ip);
        let comparisons: Vec<_> = matches
            .iter()
            .map(|entry| {
                (
                    entry
                        .run
                        .progress
                        .network_path
                        .as_ref()
                        .map(|p| p.interface_name.as_str()),
                    entry.run.results[0].tcp_ms,
                )
            })
            .collect();
        assert_eq!(
            comparisons,
            [(Some("en0"), 200.0), (Some("en7"), 40.0), (None, 22.0)]
        );
        assert!(matches!(matches[0].kind, HistoryKind::Favorite));
        assert!(matches!(matches[2].kind, HistoryKind::Scan));
    }

    #[test]
    fn history_caps_mixed_runs_and_skips_unobserved_cancellations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut older = run("old-favorite");
        older
            .results
            .push(model::IpResult::new(Ipv4Addr::LOCALHOST));
        let mut inner = RuntimeState {
            saved: SavedState {
                favorite_test: Some(FavoriteTestJob {
                    baselines: Vec::new(),
                    run: older,
                    favorite_ids: Vec::new(),
                }),
                ..Default::default()
            },
            cancellation: None,
            worker: None,
        };
        for number in 0..HISTORY_LIMIT + 1 {
            let mut pending = run(&format!("scan-{number}"));
            pending.status = "running".into();
            pending.started_at = format!("2026-11-01T00:00:{number:02}Z");
            inner
                .begin(pending.clone(), CancellationToken::new(), &path, true)
                .unwrap();
            let mut row = model::IpResult::new(Ipv4Addr::LOCALHOST);
            row.tcp_ms = 20.0;
            let (_, warning) = inner
                .complete_scan(
                    &pending.id,
                    Ok(EngineOutcome {
                        progress: pending.progress,
                        results: vec![row],
                        cancelled: false,
                    }),
                    false,
                    &path,
                )
                .unwrap();
            assert!(warning.is_none());
            if number == 0 {
                inner
                    .saved
                    .set_history_baseline("127.0.0.1", Some(&pending.id))
                    .unwrap();
            }
        }
        let mut pending = run("empty-interruption");
        pending.status = "running".into();
        pending.started_at = "2026-11-01T00:01:00Z".into();
        pending
            .results
            .push(model::IpResult::new(Ipv4Addr::LOCALHOST));
        inner
            .begin(pending, CancellationToken::new(), &path, true)
            .unwrap();
        inner.interrupt_for_shutdown(&path).unwrap();

        let mut restored = storage::load(&path).unwrap();
        let latest = restored.recent_history();
        assert_eq!(latest.len(), HISTORY_LIMIT);
        assert_eq!(restored.scan_history.len(), HISTORY_LIMIT);
        assert_eq!(
            std::fs::read_dir(path.with_extension("history"))
                .unwrap()
                .count(),
            HISTORY_LIMIT
        );
        assert_eq!(latest[0].1.id, format!("scan-{HISTORY_LIMIT}"));
        assert_eq!(latest[HISTORY_LIMIT - 1].1.id, "scan-1");
        let matches = restored.history_comparison("127.0.0.1");
        assert_eq!(matches.len(), HISTORY_LIMIT + 1);
        assert_eq!(matches[0].entry.run.id, "scan-0");
        assert_eq!(matches[0].entry.run.results[0].tcp_ms, 20.0);
        assert!(matches[0].baseline && matches[0].baseline_only);
        restored.set_history_baseline("127.0.0.1", None).unwrap();
        storage::save(&path, &restored).unwrap();
        assert!(storage::load(&path).unwrap().history_baselines.is_empty());
        assert_eq!(restored.last_run.unwrap().id, "empty-interruption");
        assert_eq!(restored.favorite_test.unwrap().run.id, "old-favorite");
    }

    #[test]
    fn every_favorite_crud_commit_allocates_a_persisted_revision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut inner = RuntimeState {
            saved: SavedState::default(),
            cancellation: None,
            worker: None,
        };
        inner.saved.favorites.revision = 40;
        let mut previous_run = run("main");
        let ip = "104.16.1.1";
        previous_run
            .results
            .push(model::IpResult::new(ip.parse().unwrap()));
        inner.saved.last_run = Some(previous_run);
        let original_folder = inner.saved.favorites.folders[0].id.clone();
        let snapshot = inner
            .update_favorites(&path, |_, favorites| favorites.create_folder("新增"))
            .unwrap();
        assert_eq!(snapshot.revision, 41);
        let folder = snapshot.folders.last().unwrap().id.clone();
        let snapshot = inner
            .update_favorites(&path, |_, favorites| {
                favorites.rename_folder(&folder, "已重命名")
            })
            .unwrap();
        assert_eq!(snapshot.revision, 42);
        let snapshot = inner
            .update_favorites(&path, |saved, favorites| {
                favorites.save_snapshot(&folder, saved.last_run.as_ref().unwrap(), ip, "保存")
            })
            .unwrap();
        assert_eq!(snapshot.revision, 43);
        let id = snapshot.entries[0].id.clone();
        let snapshot = inner
            .update_favorites(&path, |_, favorites| {
                favorites.update_entry(&id, &original_folder, "移动及备注")
            })
            .unwrap();
        assert_eq!(snapshot.revision, 44);
        let snapshot = inner
            .update_favorites(&path, |_, favorites| favorites.delete_entry(&id))
            .unwrap();
        assert_eq!(snapshot.revision, 45);
        let snapshot = inner
            .update_favorites(&path, |saved, favorites| {
                favorites.save_snapshot(&folder, saved.last_run.as_ref().unwrap(), ip, "再次保存")
            })
            .unwrap();
        assert_eq!(snapshot.revision, 46);
        let snapshot = inner
            .update_favorites(&path, |_, favorites| favorites.delete_folder(&folder))
            .unwrap();
        assert_eq!(snapshot.revision, 47);
        let persisted = storage::load(&path).unwrap().favorites;
        assert_eq!(
            serde_json::to_value(persisted).unwrap(),
            serde_json::to_value(snapshot).unwrap()
        );
        // Reads and a normal scan do not allocate a library revision.
        inner
            .begin(run("other-main"), CancellationToken::new(), &path, true)
            .unwrap();
        assert_eq!(inner.saved.favorites.revision, 47);
        assert_eq!(storage::load(&path).unwrap().favorites.revision, 47);
    }

    #[test]
    fn download_only_persists_downloads_and_preserves_full_snapshots_and_unstarted_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut inner = RuntimeState {
            saved: SavedState::default(),
            cancellation: None,
            worker: None,
        };
        let config = ScanConfig::default();
        let folder = inner.saved.favorites.folders[0].id.clone();
        let mut full = run("full");
        for ip in ["104.16.1.1", "104.16.1.2", "104.16.1.3"] {
            let mut row = model::IpResult::new(ip.parse().unwrap());
            row.tcp_stability = Some(stability::summarize(10, 200.0, vec![Some(50.0); 10]));
            row.download_samples_mbps = vec![30.0, 30.0];
            row.download_mbps = 30.0;
            full.results.push(row);
            inner
                .saved
                .favorites
                .save_snapshot(&folder, &full, ip, "")
                .unwrap();
        }
        let original = serde_json::to_value(&inner.saved.favorites.entries).unwrap();
        let (started, ips) = inner
            .begin_favorite_test(
                config,
                true,
                Some(&folder),
                None,
                None,
                None,
                CancellationToken::new(),
                &path,
            )
            .unwrap();
        assert_eq!(ips.len(), 3);
        assert_eq!(started.job.run.progress.stage, "downloading");
        let mut results = started.job.run.results.clone();
        results[0].download_samples_mbps = vec![47.0, 49.0];
        results[0].download_mbps = 48.0;
        results[1].download_samples_mbps = vec![52.0];
        results[1].download_mbps = 52.0;
        inner
            .complete_favorite_test(
                &started.job.run.id,
                Ok(EngineOutcome {
                    results,
                    progress: started.job.run.progress.clone(),
                    cancelled: true,
                }),
                true,
                &path,
            )
            .unwrap();

        let saved = storage::load(&path).unwrap();
        let history = saved.recent_history();
        assert!(history[0].1.download_only);
        assert_eq!(
            saved.history_comparison("104.16.1.1")[0].entry.run.results[0].download_mbps,
            48.0
        );
        for (index, (entry, expected)) in saved
            .favorites
            .entries
            .iter()
            .zip([Some(48.0), Some(52.0), None])
            .enumerate()
        {
            assert_eq!(
                entry
                    .download_test
                    .as_ref()
                    .map(|test| test.result.download_mbps),
                expected
            );
            let mut full_snapshot = entry.clone();
            full_snapshot.download_test = None;
            assert_eq!(
                serde_json::to_value(full_snapshot).unwrap(),
                original[index]
            );
        }
    }

    #[test]
    fn start_completion_and_later_note_snapshots_have_a_durable_total_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut inner = RuntimeState {
            saved: SavedState::default(),
            cancellation: None,
            worker: None,
        };
        let folder = inner.saved.favorites.folders[0].id.clone();
        let (late_start_response, _) = inner
            .begin_favorite_test(
                ScanConfig::default(),
                false,
                Some(&folder),
                None,
                Some("104.16.1.1"),
                Some("手动添加"),
                CancellationToken::new(),
                &path,
            )
            .unwrap();
        assert_eq!(late_start_response.favorites.revision, 1);
        assert_eq!(
            late_start_response.favorites.entries[0].result.region_code,
            "UNKNOWN"
        );
        let id = late_start_response.favorites.entries[0].id.clone();
        let mut rows = late_start_response.job.run.results.clone();
        rows[0].tcp_ms = 27.0;
        rows[0].region_code = "JP".into();
        rows[0].colo = "NRT".into();
        let (completed_event, warning) = inner
            .complete_favorite_test(
                &late_start_response.job.run.id,
                Ok(EngineOutcome {
                    progress: ScanProgress::default(),
                    results: rows,
                    cancelled: false,
                }),
                false,
                &path,
            )
            .unwrap();
        assert!(warning.is_none());
        assert_eq!(completed_event.favorites.revision, 2);
        assert_eq!(completed_event.favorites.entries[0].id, id);
        assert_eq!(completed_event.favorites.entries[0].result.colo, "NRT");
        // The command's earlier whole-library response may arrive after the
        // terminal event. Its lower revision makes that ordering detectable.
        assert!(late_start_response.favorites.revision < completed_event.favorites.revision);
        let (note_update, _) = inner
            .begin_favorite_test(
                ScanConfig::default(),
                false,
                Some(&folder),
                None,
                Some("104.16.1.1"),
                Some("更新备注"),
                CancellationToken::new(),
                &path,
            )
            .unwrap();
        assert_eq!(note_update.favorites.revision, 3);
        assert_eq!(note_update.favorites.entries[0].note, "更新备注");
        assert_eq!(note_update.favorites.entries[0].result.colo, "NRT");
        let (cancelled_event, warning) = inner
            .complete_favorite_test(
                &note_update.job.run.id,
                Err("cancelled".into()),
                true,
                &path,
            )
            .unwrap();
        assert!(warning.is_none());
        assert_eq!(cancelled_event.favorites.revision, 4);
        // An unattempted existing row retains its latest snapshot on cancel.
        assert_eq!(cancelled_event.favorites.entries[0].result.colo, "NRT");
        assert_eq!(cancelled_event.favorites.entries[0].note, "更新备注");
        assert!(completed_event.favorites.revision < cancelled_event.favorites.revision);
        assert_eq!(storage::load(&path).unwrap().favorites.revision, 4);
    }

    #[test]
    fn favorite_revision_is_not_consumed_on_invalid_edits_or_storage_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let blocking = dir.path().join("blocking");
        std::fs::write(&blocking, "occupied").unwrap();
        let mut inner = RuntimeState {
            saved: SavedState::default(),
            cancellation: None,
            worker: None,
        };
        inner.saved.favorites.revision = 7;
        let original = serde_json::to_value(&inner.saved).unwrap();
        assert!(inner
            .update_favorites(&path, |_, favorites| favorites.create_folder(" "))
            .is_err());
        assert!(inner
            .update_favorites(&blocking.join("state.json"), |_, favorites| favorites
                .create_folder("写入失败"))
            .is_err());
        assert_eq!(serde_json::to_value(&inner.saved).unwrap(), original);
        let committed = inner
            .update_favorites(&path, |_, favorites| favorites.create_folder("成功"))
            .unwrap();
        assert_eq!(committed.revision, 8);
        inner.saved.favorites.revision = 9_007_199_254_740_991;
        let original = serde_json::to_value(&inner.saved).unwrap();
        let persisted = std::fs::read(&path).unwrap();
        assert!(inner
            .update_favorites(&path, |_, favorites| favorites
                .create_folder("超出安全整数"))
            .is_err());
        assert_eq!(serde_json::to_value(&inner.saved).unwrap(), original);
        assert_eq!(std::fs::read(&path).unwrap(), persisted);
    }

    #[test]
    fn shutdown_favorite_recovery_commits_a_revision_and_rolls_back_failed_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let blocking = dir.path().join("blocking");
        std::fs::write(&blocking, "occupied").unwrap();
        let mut inner = RuntimeState {
            saved: SavedState::default(),
            cancellation: None,
            worker: None,
        };
        let folder = inner.saved.favorites.folders[0].id.clone();
        let token = CancellationToken::new();
        let (started, _) = inner
            .begin_favorite_test(
                ScanConfig::default(),
                false,
                Some(&folder),
                None,
                Some("104.16.1.1"),
                None,
                token.clone(),
                &path,
            )
            .unwrap();
        inner.saved.favorite_test.as_mut().unwrap().run.results[0].tcp_ms = 42.0;
        let original = serde_json::to_value(&inner.saved.favorites).unwrap();
        let persisted = std::fs::read(&path).unwrap();
        assert!(inner
            .interrupt_for_shutdown(&blocking.join("state.json"))
            .is_err());
        assert!(token.is_cancelled());
        assert_eq!(
            serde_json::to_value(&inner.saved.favorites).unwrap(),
            original
        );
        assert_eq!(std::fs::read(&path).unwrap(), persisted);
        inner.interrupt_for_shutdown(&path).unwrap();
        assert_eq!(
            inner.saved.favorites.revision,
            started.favorites.revision + 1
        );
        assert_eq!(inner.saved.favorites.entries[0].result.tcp_ms, 42.0);
        assert_eq!(inner.saved.favorites.entries[0].run_status, "cancelled");
        let recovered = serde_json::to_value(&inner.saved.favorites).unwrap();
        assert_eq!(
            serde_json::to_value(storage::load(&path).unwrap().favorites).unwrap(),
            recovered
        );
        inner.interrupt_for_shutdown(&path).unwrap();
        assert_eq!(inner.saved.favorites.revision, 2);
    }

    #[test]
    fn a_failed_start_does_not_discard_previous_results() {
        let dir = tempfile::tempdir().unwrap();
        let blocking_file = dir.path().join("not-a-directory");
        std::fs::write(&blocking_file, "occupied").unwrap();
        let mut inner = RuntimeState {
            saved: SavedState {
                last_run: Some(run("old")),
                ..Default::default()
            },
            cancellation: None,
            worker: None,
        };
        let mut next = run("new");
        next.status = "running".into();
        assert!(inner
            .begin(
                next,
                CancellationToken::new(),
                &blocking_file.join("state.json"),
                true,
            )
            .is_err());
        assert_eq!(inner.saved.last_run.unwrap().id, "old");
        assert!(inner.cancellation.is_none());
    }

    #[test]
    fn invalid_candidate_sources_preserve_previous_history_and_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut previous = run("previous-successful-run");
        previous
            .results
            .push(model::IpResult::new("104.16.1.1".parse().unwrap()));
        let mut inner = RuntimeState {
            saved: SavedState {
                last_run: Some(previous),
                ..SavedState::default()
            },
            cancellation: None,
            worker: None,
        };
        storage::save(&path, &inner.saved).unwrap();
        let original_file = std::fs::read(&path).unwrap();
        let original_state = serde_json::to_value(&inner.saved).unwrap();
        for custom_ips in [
            "999.16.1.1".to_string(),
            "104.16.1.1/33".to_string(),
            "# comments only".to_string(),
            ", ;".to_string(),
            "104.16.1.1\n".repeat(20_001),
        ] {
            let config = ScanConfig {
                source: "custom".into(),
                custom_ips,
                ..ScanConfig::default()
            };
            let validation = config.validate_with_candidates().unwrap_err();
            let startup_error = inner
                .begin_scan(config, CancellationToken::new(), &path, true, None)
                .unwrap_err();
            assert_eq!(
                serde_json::to_value(startup_error).unwrap(),
                serde_json::json!({ "field": "customIps", "message": validation.message }),
            );
            assert_eq!(serde_json::to_value(&inner.saved).unwrap(), original_state);
            assert_eq!(std::fs::read(&path).unwrap(), original_file);
            assert!(!inner.has_active_measurement());
        }
    }

    #[test]
    fn prepared_scan_start_preserves_history_when_persistence_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut inner = RuntimeState {
            saved: SavedState {
                last_run: Some(run("old")),
                ..SavedState::default()
            },
            cancellation: None,
            worker: None,
        };
        storage::save(&path, &inner.saved).unwrap();
        let original_file = std::fs::read(&path).unwrap();
        let original_state = serde_json::to_value(&inner.saved).unwrap();
        let config = ScanConfig {
            source: "custom".into(),
            custom_ips: "104.16.1.1".into(),
            ..ScanConfig::default()
        };
        assert!(inner
            .begin_scan(
                config,
                CancellationToken::new(),
                &path.join("blocked"),
                true,
                None
            )
            .is_err());
        assert_eq!(serde_json::to_value(&inner.saved).unwrap(), original_state);
        assert_eq!(std::fs::read(&path).unwrap(), original_file);
        assert!(!inner.has_active_measurement());
    }

    #[test]
    fn closing_recovered_state_keeps_the_original_corrupt_file_in_its_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let original = b"{\"favorites\": {\"entries\": [recoverable data";
        std::fs::write(&path, original).unwrap();
        let saved = storage::load_for_startup(&path).unwrap();
        assert!(saved.warning.is_some());
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|entry| entry != &path)
            .collect();
        assert_eq!(backups.len(), 1);
        let mut inner = RuntimeState {
            saved,
            cancellation: None,
            worker: None,
        };
        inner.interrupt_for_shutdown(&path).unwrap();
        assert!(storage::load(&path).is_ok());
        assert_eq!(std::fs::read(&backups[0]).unwrap(), original);
    }

    #[test]
    fn failed_favorite_write_restores_library_warning_and_running_scan() {
        let dir = tempfile::tempdir().unwrap();
        let blocking_file = dir.path().join("not-a-directory");
        std::fs::write(&blocking_file, "occupied").unwrap();
        let mut current = run("active");
        current.status = "running".into();
        current.progress.completed = 42;
        let cancellation = CancellationToken::new();
        let mut inner = RuntimeState {
            saved: SavedState {
                last_run: Some(current),
                warning: Some("previous warning".into()),
                ..Default::default()
            },
            cancellation: Some(cancellation.clone()),
            worker: None,
        };
        let original = serde_json::to_value(&inner.saved).unwrap();
        assert!(inner
            .update_favorites(&blocking_file.join("state.json"), |_, favorites| {
                favorites.create_folder("不可保存")
            })
            .is_err());
        assert_eq!(serde_json::to_value(&inner.saved).unwrap(), original);
        assert!(!cancellation.is_cancelled());
        assert!(inner.cancellation.is_some());
    }

    #[test]
    fn favorites_persist_the_latest_scan_and_survive_starting_another_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut current = run("old");
        current.progress.completed = 42;
        let mut result = model::IpResult::new("104.16.1.1".parse().unwrap());
        result.download_mbps = 35.0;
        let ip = result.ip.clone();
        current.results.push(result);
        let mut inner = RuntimeState {
            saved: SavedState {
                last_run: Some(current),
                ..Default::default()
            },
            cancellation: None,
            worker: None,
        };
        let folder = inner.saved.favorites.folders[0].id.clone();
        inner
            .update_favorites(&path, |saved, favorites| {
                favorites.save_snapshot(&folder, saved.last_run.as_ref().unwrap(), &ip, "收藏")
            })
            .unwrap();
        let favorite = serde_json::to_value(&inner.saved.favorites.entries[0]).unwrap();
        assert_eq!(
            storage::load(&path)
                .unwrap()
                .last_run
                .unwrap()
                .progress
                .completed,
            42
        );
        let mut next = run("next");
        next.status = "running".into();
        inner
            .begin(next, CancellationToken::new(), &path, true)
            .unwrap();
        assert_eq!(
            serde_json::to_value(&storage::load(&path).unwrap().favorites.entries[0]).unwrap(),
            favorite
        );
    }

    #[test]
    fn favorite_and_main_tests_are_mutually_exclusive_without_discarding_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut inner = RuntimeState {
            saved: SavedState {
                last_run: Some(run("old-main")),
                ..Default::default()
            },
            cancellation: None,
            worker: None,
        };
        let old_config = serde_json::to_value(&inner.saved.config).unwrap();
        let folder = inner.saved.favorites.folders[0].id.clone();
        inner
            .begin_favorite_test(
                ScanConfig::default(),
                false,
                Some(&folder),
                None,
                Some("104.16.1.1"),
                None,
                CancellationToken::new(),
                &path,
            )
            .unwrap();
        let mut main = run("new-main");
        main.status = "running".into();
        assert!(inner
            .begin(main.clone(), CancellationToken::new(), &path, true)
            .is_err());
        assert!(inner
            .begin_favorite_test(
                ScanConfig::default(),
                false,
                Some(&folder),
                None,
                Some("104.16.1.2"),
                None,
                CancellationToken::new(),
                &path,
            )
            .is_err());
        assert_eq!(inner.saved.last_run.as_ref().unwrap().id, "old-main");
        assert_eq!(
            serde_json::to_value(&inner.saved.config).unwrap(),
            old_config
        );
        let job = inner.saved.favorite_test.as_ref().unwrap().clone();
        inner
            .complete_favorite_test(&job.run.id, Err("cancelled".into()), true, &path)
            .unwrap();
        inner
            .begin(main, CancellationToken::new(), &path, true)
            .unwrap();
        assert!(inner
            .begin_favorite_test(
                ScanConfig::default(),
                false,
                Some(&folder),
                None,
                Some("104.16.1.2"),
                None,
                CancellationToken::new(),
                &path,
            )
            .is_err());
    }

    #[test]
    fn favorite_start_persists_pending_user_settings_without_target_source_changes() {
        for source in ["cloudflare", "custom"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.json");
            let previous_run = run("previous-main");
            let previous_history = serde_json::to_value(&previous_run).unwrap();
            let mut inner = RuntimeState {
                saved: SavedState {
                    last_run: Some(previous_run),
                    ..Default::default()
                },
                cancellation: None,
                worker: None,
            };
            let config = ScanConfig {
                max_tcp_ms: Some(231.0),
                source: source.into(),
                custom_ips: "104.20.0.0/24".into(),
                ..ScanConfig::default()
            };
            let expected_config = serde_json::to_value(&config).unwrap();
            let folder = inner.saved.favorites.folders[0].id.clone();
            let (started, _) = inner
                .begin_favorite_test(
                    config,
                    false,
                    Some(&folder),
                    None,
                    Some("104.16.1.1"),
                    None,
                    CancellationToken::new(),
                    &path,
                )
                .unwrap();
            assert_eq!(started.job.run.config.source, "custom");
            assert_eq!(started.job.run.config.custom_ips, "104.16.1.1");
            assert_eq!(
                serde_json::to_value(&inner.saved.config).unwrap(),
                expected_config
            );
            let restored = storage::load(&path).unwrap();
            assert_eq!(
                serde_json::to_value(&restored.config).unwrap(),
                expected_config
            );
            assert_eq!(
                serde_json::to_value(restored.last_run.unwrap()).unwrap(),
                previous_history
            );
        }
    }

    #[test]
    fn targeted_test_with_an_empty_source_does_not_persist_invalid_active_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut inner = RuntimeState {
            saved: SavedState::default(),
            cancellation: None,
            worker: None,
        };
        let previous_config = serde_json::to_value(&inner.saved.config).unwrap();
        let config = ScanConfig {
            source: "custom".into(),
            custom_ips: String::new(),
            max_tcp_ms: Some(231.0),
            ..ScanConfig::default()
        };
        assert!(config.validate().is_err());
        let folder = inner.saved.favorites.folders[0].id.clone();
        let (started, _) = inner
            .begin_favorite_test(
                config,
                false,
                Some(&folder),
                None,
                Some("104.16.1.1"),
                None,
                CancellationToken::new(),
                &path,
            )
            .unwrap();
        assert_eq!(started.job.run.config.max_tcp_ms, Some(231.0));
        assert_eq!(started.job.run.config.custom_ips, "104.16.1.1");
        assert_eq!(
            serde_json::to_value(&storage::load(&path).unwrap().config).unwrap(),
            previous_config
        );
    }

    #[test]
    fn failed_manual_start_rolls_back_the_placeholder_and_cancellation_token() {
        let dir = tempfile::tempdir().unwrap();
        let blocking = dir.path().join("blocking");
        std::fs::write(&blocking, "occupied").unwrap();
        let mut inner = RuntimeState {
            saved: SavedState::default(),
            cancellation: None,
            worker: None,
        };
        let folder = inner.saved.favorites.folders[0].id.clone();
        let original = serde_json::to_value(&inner.saved).unwrap();
        assert!(inner
            .begin_favorite_test(
                ScanConfig {
                    max_tcp_ms: Some(231.0),
                    ..ScanConfig::default()
                },
                false,
                Some(&folder),
                None,
                Some("104.16.1.1"),
                None,
                CancellationToken::new(),
                &blocking.join("state.json"),
            )
            .is_err());
        assert_eq!(serde_json::to_value(&inner.saved).unwrap(), original);
        assert!(inner.cancellation.is_none());
    }

    #[test]
    fn failed_retest_commit_preserves_saved_measurements_and_reports_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let blocking = dir.path().join("blocking");
        std::fs::write(&blocking, "occupied").unwrap();
        let mut previous = run("main");
        let mut row = model::IpResult::new("104.16.1.1".parse().unwrap());
        row.download_mbps = 35.0;
        previous.results.push(row);
        let mut inner = RuntimeState {
            saved: SavedState {
                last_run: Some(previous.clone()),
                ..Default::default()
            },
            cancellation: None,
            worker: None,
        };
        let folder = inner.saved.favorites.folders[0].id.clone();
        inner
            .saved
            .favorites
            .save_snapshot(&folder, &previous, "104.16.1.1", "original")
            .unwrap();
        let (started, _) = inner
            .begin_favorite_test(
                ScanConfig::default(),
                false,
                Some(&folder),
                None,
                None,
                None,
                CancellationToken::new(),
                &path,
            )
            .unwrap();
        let old = serde_json::to_value(&started.favorites).unwrap();
        let persisted = std::fs::read(&path).unwrap();
        let mut rows = started.job.run.results;
        rows[0].download_mbps = 99.0;
        let (completed, warning) = inner
            .complete_favorite_test(
                &started.job.run.id,
                Ok(EngineOutcome {
                    progress: ScanProgress::default(),
                    results: rows,
                    cancelled: false,
                }),
                false,
                &blocking.join("state.json"),
            )
            .unwrap();
        assert!(warning.unwrap().contains("原收藏数据已保留"));
        assert_eq!(serde_json::to_value(&completed.favorites).unwrap(), old);
        assert_eq!(completed.job.run.status, "completed");
        assert_eq!(completed.job.run.results[0].download_mbps, 99.0);
        assert!(inner.saved.warning.is_some());
        assert!(inner.cancellation.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), persisted);
        assert_eq!(inner.saved.last_run.as_ref().unwrap().id, "main");

        inner.interrupt_for_shutdown(&path).unwrap();
        let restored = storage::load(&path).unwrap();
        assert_eq!(restored.favorites.revision, started.favorites.revision + 1);
        assert_eq!(restored.favorites.entries[0].result.download_mbps, 99.0);
        assert!(!restored.pending_favorite_results);
    }
}
