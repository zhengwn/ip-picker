use crate::{favorites::was_attempted, model::ScanRun, network::NetworkPath};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use std::{cmp::Reverse, collections::HashSet, sync::Arc};

pub const HISTORY_LIMIT: usize = 10;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HistoryKind {
    Scan,
    Favorite,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub kind: HistoryKind,
    pub run: Arc<ScanRun>,
    #[serde(skip)]
    pub(crate) file_id: Option<uuid::Uuid>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryMatch {
    #[serde(flatten)]
    pub entry: HistoryEntry,
    pub baseline: bool,
    pub baseline_only: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistorySummary {
    kind: HistoryKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    quick_validation: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retest_ip: Option<String>,
    id: String,
    status: String,
    started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    network_path: Option<NetworkPath>,
    result_count: usize,
    target_url: String,
}

impl HistorySummary {
    pub fn new(kind: HistoryKind, run: &ScanRun) -> Self {
        Self {
            kind,
            quick_validation: run.quick_validation,
            retest_ip: run.retest_ip.clone(),
            id: run.id.clone(),
            status: run.status.clone(),
            started_at: run.started_at.clone(),
            finished_at: run.finished_at.clone(),
            network_path: run.progress.network_path.clone(),
            result_count: run.results.len(),
            target_url: run.config.target_url.clone(),
        }
    }
}

#[derive(Serialize)]
pub struct ScanHistory {
    pub limit: usize,
    pub entries: Vec<HistorySummary>,
}

pub fn has_history_results(run: &ScanRun) -> bool {
    match run.status.as_str() {
        "completed" => !run.results.is_empty(),
        "cancelled" | "failed" => run.results.iter().any(was_attempted),
        _ => false,
    }
}

pub fn recent_runs<'a>(
    archived: &'a [HistoryEntry],
    last_scan: Option<&'a ScanRun>,
    last_retest: Option<&'a ScanRun>,
) -> Vec<(HistoryKind, &'a ScanRun)> {
    let mut runs: Vec<_> = last_scan
        .map(|run| (HistoryKind::Scan, run))
        .into_iter()
        .chain(last_retest.map(|run| (HistoryKind::Favorite, run)))
        .chain(
            archived
                .iter()
                .map(|entry| (entry.kind, entry.run.as_ref())),
        )
        .filter(|(_, run)| has_history_results(run))
        .collect();
    runs.sort_by_cached_key(|(_, run)| Reverse(DateTime::parse_from_rfc3339(&run.started_at).ok()));
    let mut seen = HashSet::new();
    runs.retain(|(_, run)| seen.insert(run.id.as_str()));
    runs.truncate(HISTORY_LIMIT);
    runs
}

pub fn compare_ip(runs: Vec<(HistoryKind, &ScanRun)>, ip: &str) -> Vec<HistoryEntry> {
    runs.into_iter()
        .filter_map(|(kind, run)| {
            let result = run.results.iter().find(|result| {
                result.ip == ip && (run.status == "completed" || was_attempted(result))
            })?;
            let comparison = ScanRun {
                quick_validation: run.quick_validation,
                retest_ip: run.retest_ip.clone(),
                id: run.id.clone(),
                status: run.status.clone(),
                started_at: run.started_at.clone(),
                finished_at: run.finished_at.clone(),
                config: run.config.clone(),
                progress: run.progress.clone(),
                results: vec![result.clone()],
                error: run.error.clone(),
            };
            Some(HistoryEntry {
                kind,
                run: Arc::new(comparison),
                file_id: None,
            })
        })
        .collect()
}
