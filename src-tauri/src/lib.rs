pub mod engine;
mod favorites;
pub mod model;
pub mod network;
pub mod stability;
mod storage;

use crate::{
    engine::{run_scan, run_targeted_scan, EngineOutcome, EngineUpdate, UpdateCallback},
    favorites::{FavoriteLibrary, FavoriteTestJob, FavoriteTestStart},
    model::{ScanConfig, ScanProgress, ScanRun},
    storage::SavedState,
};
use std::{
    net::Ipv4Addr,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tauri::{Emitter, Manager, State};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tokio_util::sync::CancellationToken;

/// Keep slow UI serialization out of the network task. The single watch slot
/// retains only the newest snapshot while an earlier snapshot is being sent.
fn progress_bridge<F>(deliver: F) -> (UpdateCallback, tokio::task::JoinHandle<()>)
where
    F: Fn(EngineUpdate) + Send + Sync + 'static,
{
    let (sender, mut receiver) = tokio::sync::watch::channel::<Option<EngineUpdate>>(None);
    let callback: UpdateCallback = Arc::new(move |update| {
        sender.send_replace(Some(update));
    });
    let deliver = Arc::new(deliver);
    let forwarder = tokio::spawn(async move {
        while receiver.changed().await.is_ok() {
            let update = receiver.borrow_and_update().clone();
            if let Some(update) = update {
                let deliver = deliver.clone();
                if tokio::task::spawn_blocking(move || deliver(update))
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

struct RuntimeState {
    saved: SavedState,
    cancellation: Option<CancellationToken>,
}

impl RuntimeState {
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
        storage::save(path, &next)?;
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
        change(&self.saved, &mut next.favorites)?;
        self.commit_favorites(next, path)
    }

    fn begin(
        &mut self,
        run: ScanRun,
        token: CancellationToken,
        path: &std::path::Path,
    ) -> Result<(), String> {
        if self.has_active_measurement() {
            return Err("已有测试正在运行，请先停止并等待结束".into());
        }
        let old_saved = self.saved.clone();
        let old_token = self.cancellation.clone();
        self.saved.config = run.config.clone();
        self.saved.last_run = Some(run);
        self.saved.warning = None;
        self.cancellation = Some(token);
        if let Err(error) = storage::save(path, &self.saved) {
            self.saved = old_saved;
            self.cancellation = old_token;
            return Err(error);
        }
        Ok(())
    }

    fn begin_favorite_test(
        &mut self,
        config: ScanConfig,
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
        let (job, ips) = next.favorites.prepare_test(
            ScanRun {
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
        next.favorite_test = Some(job.clone());
        let favorites = self.commit_favorites(next, path)?;
        self.cancellation = Some(token);
        Ok((FavoriteTestStart { favorites, job }, ips))
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
        let mut next = self.saved.clone();
        next.favorites
            .apply_test_results(next.favorite_test.as_ref().unwrap());
        let warning = match self.commit_favorites(next, path) {
            Ok(_) => None,
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
                }
            }
        }
        if let Some(job) = next
            .favorite_test
            .as_mut()
            .filter(|job| job.run.status == "running")
        {
            job.interrupt("关闭应用时收藏测速中断，已保留已完成数据");
            next.favorites.apply_test_results(job);
            self.commit_favorites(next, path)?;
        } else {
            storage::save(path, &next)?;
            self.saved = next;
        }
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
    run.progress.stage = run.status.clone();
    run.progress.stability_batch = None;
    run.progress.precision_batch = None;
    run.finished_at = Some(chrono::Utc::now().to_rfc3339());
    if run.status != "completed" {
        for result in &mut run.results {
            result.role = Some("candidate".into());
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
}

#[tauri::command]
fn load_state(state: State<'_, AppState>) -> Result<SavedState, String> {
    Ok(state
        .inner
        .lock()
        .map_err(|_| "应用状态不可用")?
        .saved
        .clone())
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
fn save_config(config: ScanConfig, state: State<'_, AppState>) -> Result<(), String> {
    config.validate()?;
    let mut inner = state.inner.lock().map_err(|_| "应用状态不可用")?;
    let old = inner.saved.clone();
    inner.saved.config = config;
    inner.saved.warning = None;
    if let Err(error) = storage::save(&state.storage_path, &inner.saved) {
        inner.saved = old;
        return Err(error);
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
fn delete_favorites_for_ip(
    ip: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<FavoriteLibrary, String> {
    let favorites = state
        .inner
        .lock()
        .map_err(|_| "应用状态不可用")?
        .update_favorites(&state.storage_path, |saved, favorites| {
            if saved
                .favorite_test
                .as_ref()
                .is_some_and(|job| job.run.status == "running")
            {
                return Err("收藏复测正在进行，请结束后取消收藏".into());
            }
            favorites.delete_ip(&ip)
        })?;
    let _ = app.emit("favorites-update", favorites.clone());
    Ok(favorites)
}

#[tauri::command]
async fn start_favorite_test(
    config: ScanConfig,
    folder_id: Option<String>,
    favorite_id: Option<String>,
    ip: Option<String>,
    note: Option<String>,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<FavoriteTestStart, String> {
    let cancellation = CancellationToken::new();
    let (started, ips) = state
        .inner
        .lock()
        .map_err(|_| "应用状态不可用")?
        .begin_favorite_test(
            config,
            folder_id.as_deref(),
            favorite_id.as_deref(),
            ip.as_deref(),
            note.as_deref(),
            cancellation.clone(),
            &state.storage_path,
        )?;
    let _ = app.emit("favorites-update", started.favorites.clone());
    let _ = app.emit("favorite-test-update", started.job.clone());
    let owned_state = state.inner().clone();
    let run_id = started.job.run.id.clone();
    let config = started.job.run.config.clone();
    tauri::async_runtime::spawn(async move {
        let callback_state = owned_state.clone();
        let callback_app = app.clone();
        let callback_id = run_id.clone();
        let (callback, forwarder) = progress_bridge(move |update: EngineUpdate| {
            let snapshot = callback_state.inner.lock().ok().and_then(|mut inner| {
                let current = inner.saved.favorite_test.as_mut()?;
                if current.run.id != callback_id || current.run.status != "running" {
                    return None;
                }
                current.run.progress = update.progress;
                current.run.results = update.results;
                current.record_observed_regions();
                Some(current.clone())
            });
            if let Some(snapshot) = snapshot {
                let _ = callback_app.emit("favorite-test-update", snapshot);
            }
        });
        let outcome = run_targeted_scan(config, ips, cancellation.clone(), callback).await;
        let _ = forwarder.await;
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
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<ScanRun, String> {
    config.validate()?;
    let cancellation = CancellationToken::new();
    let run = ScanRun {
        id: uuid::Uuid::new_v4().to_string(),
        status: "running".into(),
        started_at: chrono::Utc::now().to_rfc3339(),
        finished_at: None,
        config: config.clone(),
        progress: ScanProgress::default(),
        results: Vec::new(),
        error: None,
    };
    {
        let mut inner = state.inner.lock().map_err(|_| "应用状态不可用")?;
        inner.begin(run.clone(), cancellation.clone(), &state.storage_path)?;
    }
    let owned_state = state.inner().clone();
    let run_id = run.id.clone();
    tauri::async_runtime::spawn(async move {
        let callback_state = owned_state.clone();
        let callback_app = app.clone();
        let callback_id = run_id.clone();
        let (callback, forwarder) = progress_bridge(move |update: EngineUpdate| {
            let snapshot = callback_state.inner.lock().ok().and_then(|mut inner| {
                let current = inner.saved.last_run.as_mut()?;
                if current.id != callback_id || current.status != "running" {
                    return None;
                }
                current.progress = update.progress;
                current.results = update.results;
                Some(current.clone())
            });
            // Serialization can be expensive; never retain the state lock here.
            if let Some(snapshot) = snapshot {
                let _ = callback_app.emit("scan-update", snapshot);
            }
        });
        let outcome = run_scan(config, cancellation.clone(), callback).await;
        // run_scan drops its callback sender. Drain the last queued snapshot
        // before publishing the authoritative completed/cancelled result.
        let _ = forwarder.await;
        if let Ok(mut inner) = owned_state.inner.lock() {
            if let Some(current) = inner.saved.last_run.as_mut() {
                if current.id != run_id {
                    return;
                }
                match outcome {
                    Ok(outcome) => {
                        current.progress = outcome.progress;
                        current.results = outcome.results;
                        current.status = if cancellation.is_cancelled() {
                            "cancelled"
                        } else {
                            "completed"
                        }
                        .into();
                    }
                    Err(error) => {
                        current.status = if cancellation.is_cancelled() {
                            "cancelled"
                        } else {
                            "failed"
                        }
                        .into();
                        current.error = Some(error);
                    }
                }
                current.progress.stage = current.status.clone();
                current.progress.stability_batch = None;
                current.progress.precision_batch = None;
                current.finished_at = Some(chrono::Utc::now().to_rfc3339());
                if current.status != "completed" {
                    for result in &mut current.results {
                        result.role = Some("candidate".into());
                    }
                    current.progress.message = if current.status == "cancelled" {
                        "测试已停止，保留已完成数据"
                    } else {
                        "测试失败，请查看错误详情"
                    }
                    .into();
                }
            }
            inner.cancellation = None;
            if let Err(error) = storage::save(&owned_state.storage_path, &inner.saved) {
                inner.saved.warning = Some(error.clone());
                let _ = app.emit(
                    "storage-warning",
                    format!("本轮结果尚未保存到本地，可先复制需要的 IP：{error}"),
                );
            }
            if let Some(current) = inner.saved.last_run.as_ref() {
                let _ = app.emit("scan-update", current.clone());
            }
        }
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
    tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            let path = app.path().app_data_dir()?.join("state.json");
            let saved = storage::load(&path).unwrap_or_else(|error| SavedState {
                warning: Some(error),
                ..SavedState::default()
            });
            app.manage(AppState {
                inner: Arc::new(Mutex::new(RuntimeState {
                    saved,
                    cancellation: None,
                })),
                storage_path: path,
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::CloseRequested { .. }) {
                let state = window.state::<AppState>();
                if let Ok(mut inner) = state.inner.lock() {
                    let _ = inner.interrupt_for_shutdown(&state.storage_path);
                };
            }
        })
        .invoke_handler(tauri::generate_handler![
            load_state,
            get_scan,
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
            update_favorite,
            delete_favorite,
            delete_favorites_for_ip,
            copy_text
        ])
        .run(tauri::generate_context!())
        .expect("IP Picker 启动失败");
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_progress_delivery_keeps_only_the_latest_snapshot_without_blocking_producers() {
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let received = delivered.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Mutex::new(Some(started_tx));
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let (callback, forwarder) = progress_bridge(move |update| {
            let number = update.progress.completed;
            received.lock().unwrap().push(number);
            if number == 1 {
                started_tx.lock().unwrap().take().unwrap().send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap();
            }
        });
        let update = |number| EngineUpdate {
            progress: ScanProgress {
                completed: number,
                ..Default::default()
            },
            results: Vec::new(),
        };
        callback(update(1));
        started_rx.await.unwrap();
        for number in 2..=100 {
            callback(update(number));
        }
        drop(callback);
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), forwarder)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*delivered.lock().unwrap(), vec![1, 100]);
    }

    fn run(id: &str) -> ScanRun {
        ScanRun {
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

    #[test]
    fn every_favorite_crud_commit_allocates_a_persisted_revision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut inner = RuntimeState {
            saved: SavedState::default(),
            cancellation: None,
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
            .update_favorites(&path, |_, favorites| favorites.delete_ip(ip))
            .unwrap();
        assert_eq!(snapshot.revision, 47);
        let snapshot = inner
            .update_favorites(&path, |_, favorites| favorites.delete_folder(&folder))
            .unwrap();
        assert_eq!(snapshot.revision, 48);
        let persisted = storage::load(&path).unwrap().favorites;
        assert_eq!(
            serde_json::to_value(persisted).unwrap(),
            serde_json::to_value(snapshot).unwrap()
        );
        // Reads and a normal scan do not allocate a library revision.
        inner
            .begin(run("other-main"), CancellationToken::new(), &path)
            .unwrap();
        assert_eq!(inner.saved.favorites.revision, 48);
        assert_eq!(storage::load(&path).unwrap().favorites.revision, 48);
    }

    #[test]
    fn start_completion_and_later_note_snapshots_have_a_durable_total_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut inner = RuntimeState {
            saved: SavedState::default(),
            cancellation: None,
        };
        let folder = inner.saved.favorites.folders[0].id.clone();
        let (late_start_response, _) = inner
            .begin_favorite_test(
                ScanConfig::default(),
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
        };
        let folder = inner.saved.favorites.folders[0].id.clone();
        let token = CancellationToken::new();
        let (started, _) = inner
            .begin_favorite_test(
                ScanConfig::default(),
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
        };
        let mut next = run("new");
        next.status = "running".into();
        assert!(inner
            .begin(
                next,
                CancellationToken::new(),
                &blocking_file.join("state.json")
            )
            .is_err());
        assert_eq!(inner.saved.last_run.unwrap().id, "old");
        assert!(inner.cancellation.is_none());
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
        inner.begin(next, CancellationToken::new(), &path).unwrap();
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
        };
        let old_config = serde_json::to_value(&inner.saved.config).unwrap();
        let folder = inner.saved.favorites.folders[0].id.clone();
        inner
            .begin_favorite_test(
                ScanConfig::default(),
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
            .begin(main.clone(), CancellationToken::new(), &path)
            .is_err());
        assert!(inner
            .begin_favorite_test(
                ScanConfig::default(),
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
        inner.begin(main, CancellationToken::new(), &path).unwrap();
        assert!(inner
            .begin_favorite_test(
                ScanConfig::default(),
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
    fn failed_manual_start_rolls_back_the_placeholder_and_cancellation_token() {
        let dir = tempfile::tempdir().unwrap();
        let blocking = dir.path().join("blocking");
        std::fs::write(&blocking, "occupied").unwrap();
        let mut inner = RuntimeState {
            saved: SavedState::default(),
            cancellation: None,
        };
        let folder = inner.saved.favorites.folders[0].id.clone();
        let original = serde_json::to_value(&inner.saved).unwrap();
        assert!(inner
            .begin_favorite_test(
                ScanConfig::default(),
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
    }
}
