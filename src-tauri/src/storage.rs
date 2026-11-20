use crate::{
    favorites::{CandidateListSource, FavoriteLibrary, FavoriteTestJob},
    history::{
        compare_ip, has_history_results, recent_runs, HistoryEntry, HistoryKind, HistoryMatch,
    },
    model::{IpResult, ScanConfig, ScanProgress, ScanRun, CLOUDFLARE_CANDIDATE_COUNT},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs, io,
    io::{BufReader, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;

const CANDIDATE_COVERAGE_VERSION: u32 = 2;
const RECOVERY_WITH_SOURCES_EXTENSION: &str = "with-sources.bak";

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
    #[serde(default, skip_serializing)]
    pub scan_history: Vec<HistoryEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub history_baselines: BTreeMap<String, HistoryEntry>,
    // On disk this field is the archive index. Once loaded, only references
    // that could not be read stay here so a settings save cannot erase them.
    #[serde(default, rename = "scanHistoryFiles", skip_serializing)]
    pub(crate) unavailable_history_files: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub measurement_revision: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub pending_favorite_results: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(skip)]
    pub(crate) needs_save_on_shutdown: bool,
}

impl Default for SavedState {
    fn default() -> Self {
        Self {
            candidate_coverage_version: CANDIDATE_COVERAGE_VERSION,
            config: ScanConfig::default(),
            last_run: None,
            favorites: FavoriteLibrary::default(),
            favorite_test: None,
            scan_history: Vec::new(),
            history_baselines: BTreeMap::new(),
            unavailable_history_files: Vec::new(),
            measurement_revision: 0,
            pending_favorite_results: false,
            warning: None,
            needs_save_on_shutdown: false,
        }
    }
}

impl SavedState {
    pub fn apply_pending_favorite_results(&mut self) {
        if self.pending_favorite_results {
            if let Some(job) = &self.favorite_test {
                self.favorites.apply_test_results(job);
            }
            self.pending_favorite_results = false;
        }
    }

    pub fn recent_history(&self) -> Vec<(HistoryKind, &ScanRun)> {
        recent_runs(
            &self.scan_history,
            self.last_run.as_ref(),
            self.favorite_test.as_ref().map(|job| &job.run),
        )
    }

    pub fn history_comparison(&self, ip: &str) -> Vec<HistoryMatch> {
        let mut matches: Vec<_> = compare_ip(self.recent_history(), ip)
            .into_iter()
            .map(|entry| HistoryMatch {
                entry,
                baseline: false,
                baseline_only: false,
            })
            .collect();
        if let Some(baseline) = self.history_baselines.get(ip) {
            let recent = matches
                .iter()
                .position(|item| item.entry.run.id == baseline.run.id);
            if let Some(index) = recent {
                matches.remove(index);
            }
            matches.insert(
                0,
                HistoryMatch {
                    entry: baseline.clone(),
                    baseline: true,
                    baseline_only: recent.is_none(),
                },
            );
        }
        matches
    }

    pub fn set_history_baseline(&mut self, ip: &str, run_id: Option<&str>) -> Result<(), String> {
        if let Some(run_id) = run_id {
            let source = self
                .recent_history()
                .into_iter()
                .filter(|(_, run)| run.id == run_id)
                .collect();
            let baseline = compare_ip(source, ip)
                .into_iter()
                .next()
                .filter(|entry| crate::favorites::was_attempted(&entry.run.results[0]))
                .ok_or("该轮没有可固定的 IP 测量，或记录已移出测速历史")?;
            self.history_baselines.insert(ip.to_owned(), baseline);
        } else {
            self.history_baselines.remove(ip);
        }
        Ok(())
    }

    // The latest scan and retest already live in their own state slots. Keep
    // one copy of each run by moving it to the archive only on replacement.
    pub fn archive_run(&mut self, kind: HistoryKind, run: ScanRun) {
        if has_history_results(&run) {
            self.scan_history.push(HistoryEntry {
                kind,
                run: Arc::new(run),
                file_id: Some(Uuid::new_v4()),
            });
            self.retain_recent_history();
        }
    }

    pub fn retain_recent_history(&mut self) {
        let mut recent_ids: HashSet<_> = self
            .recent_history()
            .into_iter()
            .map(|(_, run)| run.id.clone())
            .collect();
        if let Some(run) = &self.last_run {
            recent_ids.remove(&run.id);
        }
        if let Some(job) = &self.favorite_test {
            recent_ids.remove(&job.run.id);
        }
        self.scan_history
            .retain(|entry| recent_ids.remove(&entry.run.id));
    }
}

fn history_path(path: &Path, id: Uuid) -> PathBuf {
    path.with_extension("history").join(format!("{id}.json"))
}

fn favorite_sources_dir(path: &Path) -> PathBuf {
    path.with_extension("favorite-sources")
}

fn favorite_source_path(path: &Path, id: Uuid) -> PathBuf {
    favorite_sources_dir(path).join(format!("{id}.txt"))
}

fn recovery_favorite_source_path(path: &Path, id: Uuid) -> PathBuf {
    path.with_extension("recovery-favorite-sources")
        .join(format!("{id}.txt"))
}

fn restore_favorite_sources(path: &Path, saved: &mut SavedState) {
    let mut empty_sources = BTreeMap::new();
    for run in saved.last_run.iter().chain(
        saved
            .scan_history
            .iter()
            .filter(|entry| matches!(entry.kind, HistoryKind::Scan))
            .map(|entry| entry.run.as_ref()),
    ) {
        let recorded_empty = !run.config.candidate_source_unrecorded
            && run.config.source == "cloudflare"
            && run.config.custom_ips.is_empty();
        empty_sources
            .entry(run.id.as_str())
            .and_modify(|empty| *empty &= recorded_empty)
            .or_insert(recorded_empty);
    }
    let favorites = &mut saved.favorites;
    // Older versions preserve runId when saving a favorite, even though they
    // drop the candidate-list fields from state.json.
    favorites
        .candidate_lists
        .retain(|_, source| !source.is_empty());
    let mut checked = HashSet::new();
    let mut missing = BTreeMap::new();
    for entry in &mut favorites.entries {
        let Some(reference) = entry.candidate_source_reference() else {
            continue;
        };
        let Ok(id) = Uuid::parse_str(reference) else {
            continue;
        };
        if !favorites.candidate_lists.contains_key(reference) && !checked.contains(reference) {
            checked.insert(reference.to_owned());
            if let Some(source) = read_favorite_source(&favorite_source_path(path, id))
                .or_else(|| read_favorite_source(&recovery_favorite_source_path(path, id)))
            {
                favorites
                    .candidate_lists
                    .insert(reference.into(), Arc::from(source));
            }
        }
        if favorites.candidate_lists.contains_key(reference) {
            continue;
        }
        if !entry.config.custom_ips.is_empty() {
            continue;
        }
        // A legacy null might have been inferred from defaulted scan fields.
        // Give it a chance to recover from a backup before trusting emptiness.
        if entry.candidate_list_id.is_unknown()
            && entry.config.source == "cloudflare"
            && empty_sources.get(entry.run_id.as_str()) == Some(&true)
        {
            entry.candidate_list_id = CandidateListSource::Inline;
            entry.candidate_list_known_empty = true;
        } else if !missing.contains_key(reference) {
            missing.insert(reference.to_owned(), id);
        }
    }
    if !missing.is_empty() {
        // A restored backup can be the only copy after an older writer
        // discards the embedded lists again. Read it without changing it.
        let ids = missing.values().copied().collect();
        let sources = embedded_recovery_sources(path, &ids);
        for (reference, id) in missing {
            if let Some(source) = sources.get(&id) {
                favorites.candidate_lists.insert(reference, source.clone());
            }
        }
    }
}

fn read_favorite_source(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .filter(|source| !source.is_empty())
}

fn preserve_favorite_sources(
    path: &Path,
    favorites: &FavoriteLibrary,
    verify_existing: bool,
) -> Result<(), String> {
    for (id, source) in &favorites.candidate_lists {
        let Ok(id) = Uuid::parse_str(id) else {
            continue;
        };
        let source_path = favorite_source_path(path, id);
        match fs::metadata(&source_path) {
            Ok(metadata) if metadata.is_file() => {
                let matches = if verify_existing {
                    metadata.len() == source.len() as u64
                        && fs::read(&source_path)
                            .map_err(|error| format!("无法读取收藏候选列表备份：{error}"))?
                            == source.as_bytes()
                } else {
                    metadata.len() > 0
                };
                if !matches {
                    write_atomic(&source_path, source.as_bytes())?;
                }
            }
            Ok(_) => return Err("收藏候选列表的备份路径不是文件".into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                write_atomic(&source_path, source.as_bytes())?;
            }
            Err(error) => return Err(format!("无法检查收藏候选列表：{error}")),
        }
    }
    Ok(())
}

fn favorite_source_ids(favorites: &FavoriteLibrary) -> HashSet<Uuid> {
    favorites
        .entries
        .iter()
        .filter_map(|entry| {
            if entry.candidate_list_id.is_unknown() && !entry.config.custom_ips.is_empty() {
                return None;
            }
            let reference = entry.candidate_source_reference()?;
            Uuid::parse_str(reference).ok()
        })
        .collect()
}

fn archive_favorite_source(path: &Path, id: Uuid, source: Option<&str>) -> Result<(), String> {
    let archive = recovery_favorite_source_path(path, id);
    match fs::read_to_string(&archive) {
        Ok(archived) if archived.is_empty() => {}
        Ok(archived) if source.is_none_or(|source| source == archived) => return Ok(()),
        Ok(_) => return Err("同一轮测速的恢复备份已有不同的候选列表".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    if let Some(source) = source {
        return write_atomic(&archive, source.as_bytes());
    }
    match fs::read(favorite_source_path(path, id)) {
        Ok(bytes) if bytes.is_empty() => Ok(()),
        Ok(bytes) => write_atomic(&archive, &bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn archive_favorite_sources(path: &Path) -> Result<(), String> {
    let files = match fs::read_dir(favorite_sources_dir(path)) {
        Ok(files) => files,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    // Corrupt state may be unreadable, so the recovery backup must preserve
    // every source that it could refer to. Run IDs identify immutable lists.
    for file in files {
        let source = file.map_err(|error| error.to_string())?.path();
        if source
            .extension()
            .is_some_and(|extension| extension == "txt")
        {
            if let Some(id) = source
                .file_stem()
                .and_then(|name| name.to_str())
                .and_then(|name| Uuid::parse_str(name).ok())
            {
                archive_favorite_source(path, id, None)?;
            }
        }
    }
    Ok(())
}

fn recovery_backup_prefix(name: &std::ffi::OsStr) -> String {
    format!("{}.recovery-", name.to_string_lossy())
}

fn recovery_backup_paths(path: &Path) -> Option<impl Iterator<Item = io::Result<PathBuf>>> {
    let (parent, name) = path.parent().zip(path.file_name())?;
    let prefix = recovery_backup_prefix(name);
    Some(
        fs::read_dir(parent)
            .ok()?
            .filter_map(move |entry| match entry {
                Ok(entry) => {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    (name.starts_with(&prefix) && name.ends_with(".bak")).then(|| Ok(entry.path()))
                }
                Err(error) => Some(Err(error)),
            }),
    )
}

fn read_recovery_backup<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    if !fs::metadata(path)?.is_file() {
        return Ok(None);
    }
    let file = fs::File::open(path)?;
    Ok(Some(serde_json::from_reader(BufReader::new(file))?))
}

fn embedded_recovery_sources(path: &Path, ids: &HashSet<Uuid>) -> BTreeMap<Uuid, Arc<str>> {
    #[derive(Deserialize)]
    struct Backup {
        #[serde(default)]
        favorites: Favorites,
    }

    #[derive(Default, Deserialize)]
    #[serde(default, rename_all = "camelCase")]
    struct Favorites {
        candidate_lists: BTreeMap<String, serde_json::Value>,
    }

    let Some(backups) = recovery_backup_paths(path) else {
        return BTreeMap::new();
    };
    let mut sources: BTreeMap<Uuid, Option<Arc<str>>> = BTreeMap::new();
    for backup in backups.flatten() {
        let Ok(Some(backup)) = read_recovery_backup::<Backup>(&backup) else {
            continue;
        };
        for (reference, value) in backup.favorites.candidate_lists {
            let Ok(id) = Uuid::parse_str(&reference) else {
                continue;
            };
            let Some(source) = value
                .as_str()
                .filter(|source| !source.is_empty() && ids.contains(&id))
            else {
                continue;
            };
            sources
                .entry(id)
                .and_modify(|existing| {
                    // Conflicting copies do not establish which list is original.
                    if existing
                        .as_deref()
                        .is_some_and(|existing| existing != source)
                    {
                        *existing = None;
                    }
                })
                .or_insert_with(|| Some(Arc::from(source)));
        }
    }
    sources
        .into_iter()
        .filter_map(|(id, source)| source.map(|source| (id, source)))
        .collect()
}

/// None means a legacy backup could not be inspected, so its references are
/// unknown. Parse only the favorite index because unrelated fields may be corrupt.
fn legacy_recovery_source_ids(path: &Path) -> Option<HashSet<Uuid>> {
    #[derive(Deserialize)]
    struct Backup {
        #[serde(default)]
        favorites: Favorites,
    }

    #[derive(Default, Deserialize)]
    #[serde(default, rename_all = "camelCase")]
    struct Favorites {
        entries: Vec<Entry>,
        candidate_lists: BTreeMap<String, String>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Entry {
        run_id: String,
        #[serde(default)]
        candidate_list_id: CandidateListSource,
        #[serde(default)]
        candidate_list_known_empty: bool,
        #[serde(default)]
        config: Config,
    }

    #[derive(Default, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Config {
        custom_ips: Option<String>,
    }

    let completed_suffix = format!(".{RECOVERY_WITH_SOURCES_EXTENSION}");
    let mut sources = HashSet::new();
    for backup in recovery_backup_paths(path)? {
        let backup = backup.ok()?;
        if backup
            .file_name()?
            .to_string_lossy()
            .ends_with(&completed_suffix)
        {
            continue;
        }
        let Some(backup) = read_recovery_backup::<Backup>(&backup).ok()? else {
            continue;
        };
        for entry in backup.favorites.entries {
            let inline = entry
                .config
                .custom_ips
                .is_some_and(|source| !source.is_empty());
            let Some(reference) = entry
                .candidate_list_id
                .lookup_reference(&entry.run_id, !inline && !entry.candidate_list_known_empty)
            else {
                continue;
            };
            let embedded = backup
                .favorites
                .candidate_lists
                .get(reference)
                .is_some_and(|source| !source.is_empty());
            if !inline && !embedded {
                if let Ok(id) = Uuid::parse_str(reference) {
                    sources.insert(id);
                }
            }
        }
    }
    Some(sources)
}

fn load_history(path: &Path, saved: &mut SavedState) -> Option<String> {
    // Earlier versions embedded the archive in state.json. Allocate its file
    // identifiers here, and move the data on the next successful state save.
    for entry in &mut saved.scan_history {
        entry.file_id = Some(Uuid::new_v4());
    }
    let mut seen = HashSet::new();
    let mut unavailable = Vec::new();
    for reference in std::mem::take(&mut saved.unavailable_history_files) {
        if !seen.insert(reference.clone()) {
            continue;
        }
        let entry = Uuid::parse_str(&reference).ok().and_then(|id| {
            let bytes = fs::read(history_path(path, id)).ok()?;
            let mut entry: HistoryEntry = serde_json::from_slice(&bytes).ok()?;
            entry.file_id = Some(id);
            Some(entry)
        });
        match entry {
            Some(entry) => saved.scan_history.push(entry),
            None => unavailable.push(reference),
        }
    }
    saved.unavailable_history_files = unavailable;
    let missing = saved.unavailable_history_files.len();
    (missing > 0).then(|| {
        format!("有 {missing} 轮测速归档无法读取。请检查应用数据目录中的 state.history 文件夹。")
    })
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Deserialize)]
struct MeasurementCheckpoint {
    run_id: String,
    revision: u64,
    progress: ScanProgress,
    results: Vec<IpResult>,
}

fn checkpoint_path(path: &Path) -> PathBuf {
    path.with_extension("checkpoint.json")
}

pub fn save_checkpoint(path: &Path, run: &ScanRun, revision: u64) -> Result<(), String> {
    #[derive(Serialize)]
    struct Snapshot<'a> {
        run_id: &'a str,
        revision: u64,
        progress: &'a ScanProgress,
        results: &'a [IpResult],
    }

    let bytes = serde_json::to_vec(&Snapshot {
        run_id: &run.id,
        revision,
        progress: &run.progress,
        results: &run.results,
    })
    .map_err(|error| error.to_string())?;
    write_atomic(&checkpoint_path(path), &bytes)
}

pub fn clear_checkpoint(path: &Path) {
    // A surviving file cannot replace a terminal run or a different run.
    let _ = fs::remove_file(checkpoint_path(path));
}

fn restore_checkpoint(path: &Path, saved: &mut SavedState) -> Result<Option<HistoryKind>, String> {
    let (kind, run) = if let Some(run) = saved
        .last_run
        .as_mut()
        .filter(|run| run.status == "running")
    {
        (HistoryKind::Scan, run)
    } else if let Some(job) = saved
        .favorite_test
        .as_mut()
        .filter(|job| job.run.status == "running")
    {
        (HistoryKind::Favorite, &mut job.run)
    } else {
        return Ok(None);
    };

    let bytes = match fs::read(checkpoint_path(path)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let checkpoint: MeasurementCheckpoint =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if checkpoint.run_id != run.id || checkpoint.revision <= saved.measurement_revision {
        return Ok(None);
    }
    run.progress = checkpoint.progress;
    run.results = checkpoint.results;
    run.update_revision = checkpoint.revision;
    saved.measurement_revision = checkpoint.revision;
    Ok(Some(kind))
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
        Ok(mut saved) => {
            // The main index has precedence while it still contains the text.
            // Repair divergent copies before an older app might replace it.
            if let Err(error) = preserve_favorite_sources(path, &saved.favorites, true) {
                let message = format!("无法备份收藏的原始候选列表：{error}");
                saved.warning = Some(match saved.warning.take() {
                    Some(warning) => format!("{warning}；{message}"),
                    None => message,
                });
            }
            return Ok(saved);
        }
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
        .prefix(&recovery_backup_prefix(name))
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(|e| preserve_error(e.to_string()))?;
    let backup_path = backup
        .path()
        .with_extension(RECOVERY_WITH_SOURCES_EXTENSION);
    backup
        .write_all(&bytes)
        .and_then(|_| backup.flush())
        .and_then(|_| backup.as_file().sync_all())
        .map_err(|e| preserve_error(e.to_string()))?;
    archive_favorite_sources(path).map_err(preserve_error)?;
    // Publish the completed name only after its sources are durable. A
    // temporary file left by a crash must never look like a complete backup.
    backup
        .persist_noclobber(&backup_path)
        .map_err(|e| preserve_error(e.error.to_string()))?;
    Ok(SavedState {
        warning: Some(format!(
            "{error}；原始设置已备份至 {}，本次使用默认设置",
            backup_path.display()
        )),
        needs_save_on_shutdown: true,
        ..SavedState::default()
    })
}

pub fn load(path: &Path) -> Result<SavedState, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(SavedState::default()),
        Err(error) => return Err(format!("无法读取本地设置：{error}")),
    };
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
    let history_warning = load_history(path, &mut saved);
    restore_favorite_sources(path, &mut saved);
    saved.favorites.share_candidate_lists();
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
    let checkpoint = restore_checkpoint(path, &mut saved);
    if let Some(run) = saved.last_run.as_mut() {
        if run.retest_ip.is_some() {
            run.record_observed_regions();
        }
        let interrupted = run.status == "running";
        if interrupted {
            saved.needs_save_on_shutdown = true;
            run.status = "cancelled".into();
            run.progress.stage = "cancelled".into();
            run.progress.stability_batch = None;
            run.progress.precision_batch = None;
            run.progress.message = "上次关闭应用时测试中断，已保留已完成数据".into();
            run.finished_at = Some(chrono::Utc::now().to_rfc3339());
        }
        for result in &mut run.results {
            if interrupted {
                result.role = Some("candidate".into());
                result.mark_cancelled(&run.config);
            }
            // Legacy records lack measurementPassed. Recover only formerly
            // eligible rows or the exact old speed-only filter, with the full
            // verification evidence still present. Hard failures never recover.
            result.measurement_passed = result.has_complete_measurement(&run.config)
                && (result.measurement_passed
                    || result.eligible
                    || result.has_only_speed_target_exclusion());
            if interrupted {
                result.apply_recommendation_threshold(&run.config);
            }
            if !run.config.accepts_initial_tcp_ms(result.tcp_ms) {
                result.eligible = false;
                result.role = Some("candidate".into());
            }
            if let Some((code, reason)) = result.stability_exclusion_details(&run.config) {
                result.eligible = false;
                result.role = Some("candidate".into());
                if !result.is_excluded() || result.has_only_speed_target_exclusion() {
                    if run.config.stability_first || result.exclusion_code.is_some() {
                        result.exclude(code, reason);
                    } else {
                        // Legacy early rejection could be based on latency or a
                        // failure-rate threshold rather than the current rule.
                        result.exclusion_reason = Some(reason);
                    }
                }
            }
        }
    }
    recover_interrupted_favorite_test(&mut saved, path);
    match checkpoint {
        Ok(Some(HistoryKind::Scan)) => {
            let previous_history = saved.scan_history.clone();
            saved.retain_recent_history();
            match save_replacing_history(path, &saved, &previous_history) {
                Ok(()) => {
                    saved.needs_save_on_shutdown = false;
                    clear_checkpoint(path);
                }
                Err(error) => {
                    saved.scan_history = previous_history;
                    saved.warning = Some(format!("中断的测速快照尚未保存到本地：{error}"));
                }
            }
        }
        Err(error) => {
            let message = format!("无法读取中断前的测速快照，已使用主状态文件：{error}");
            saved.warning = Some(match saved.warning.take() {
                Some(warning) => format!("{warning}；{message}"),
                None => message,
            });
        }
        _ => {}
    }
    if let Some(message) = history_warning {
        saved.warning = Some(match saved.warning.take() {
            Some(warning) => format!("{warning}；{message}"),
            None => message,
        });
    }
    Ok(saved)
}

fn recover_interrupted_favorite_test(saved: &mut SavedState, path: &Path) {
    if let Some(job) = saved
        .favorite_test
        .as_mut()
        .filter(|job| job.run.status == "running")
    {
        job.interrupt("上次关闭应用时收藏测速中断，已保留已完成数据");
        saved.pending_favorite_results = true;
    }
    if !saved.pending_favorite_results {
        return;
    }
    let mut next = saved.clone();
    next.apply_pending_favorite_results();
    next.retain_recent_history();
    // Persist recovery before exposing its new library version. Reopening the
    // app then reads the same terminal job instead of generating another
    // different recovered snapshot with the same revision on each load.
    let committed = next.favorites.next_revision().and_then(|revision| {
        next.favorites.revision = revision;
        next.warning = None;
        save_favorite_changes(path, &next, saved)
    });
    match committed {
        Ok(()) => {
            next.needs_save_on_shutdown = false;
            *saved = next;
            clear_checkpoint(path);
        }
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
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct StateFile<'a> {
        #[serde(flatten)]
        state: &'a SavedState,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        scan_history_files: Vec<String>,
    }

    let mut created = Vec::new();
    let result = (|| {
        // Keep recovery copies across a failed state write. The previous
        // index may already contain a compacted favorite for this run.
        preserve_favorite_sources(path, &saved.favorites, false)?;
        let mut references = saved.unavailable_history_files.clone();
        for entry in &saved.scan_history {
            let id = entry.file_id.ok_or("测速归档缺少文件标识")?;
            let archive = history_path(path, id);
            if !archive
                .try_exists()
                .map_err(|error| format!("无法检查测速归档：{error}"))?
            {
                let bytes = serde_json::to_vec(entry).map_err(|error| error.to_string())?;
                write_atomic(&archive, &bytes)?;
                created.push(archive);
            }
            references.push(id.to_string());
        }
        // The state file commits the references only after their immutable
        // files exist. A failed state write leaves the previous index intact.
        let bytes = serde_json::to_vec_pretty(&StateFile {
            state: saved,
            scan_history_files: references,
        })
        .map_err(|error| error.to_string())?;
        write_atomic(path, &bytes)
    })();
    if result.is_err() {
        for archive in created {
            let _ = fs::remove_file(archive);
        }
    }
    result
}

pub(crate) fn save_favorite_changes(
    path: &Path,
    saved: &SavedState,
    previous: &SavedState,
) -> Result<(), String> {
    // Files absent from both indexes may belong to a separate recovery
    // backup. Only remove sources explicitly retired by this commit.
    let retained = favorite_source_ids(&saved.favorites);
    let previous_ids = favorite_source_ids(&previous.favorites);
    let mut retired = previous_ids.difference(&retained).peekable();
    // Backups created by older builds may still rely on the live copy. Only
    // archive their referenced sources unless a backup cannot be inspected.
    let recovery_sources = if retired.peek().is_some() {
        legacy_recovery_source_ids(path)
    } else {
        Some(HashSet::new())
    };
    let mut remove = Vec::new();
    for id in retired {
        if recovery_sources
            .as_ref()
            .is_none_or(|sources| sources.contains(id))
        {
            let source = previous
                .favorites
                .candidate_lists
                .get(&id.to_string())
                .map(Arc::as_ref);
            if archive_favorite_source(path, *id, source).is_err() {
                // The old index might be the only copy. Keep it until we
                // know a backup can still read that source after the commit.
                if let Some(source) = source {
                    let live = favorite_source_path(path, *id);
                    if !fs::read_to_string(&live).is_ok_and(|existing| existing == source) {
                        write_atomic(&live, source.as_bytes())?;
                    }
                }
                continue;
            }
        }
        remove.push(*id);
    }
    save_replacing_history(path, saved, &previous.scan_history)?;
    for id in remove {
        let _ = fs::remove_file(favorite_source_path(path, id));
    }
    Ok(())
}

pub fn save_replacing_history(
    path: &Path,
    saved: &SavedState,
    previous: &[HistoryEntry],
) -> Result<(), String> {
    save(path, saved)?;
    let retained: HashSet<_> = saved
        .scan_history
        .iter()
        .filter_map(|entry| entry.file_id)
        .collect();
    for id in previous.iter().filter_map(|entry| entry.file_id) {
        if !retained.contains(&id) {
            let _ = fs::remove_file(history_path(path, id));
        }
    }
    Ok(())
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

    #[test]
    fn migrates_inline_history_without_rewriting_archives_on_settings_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let run = legacy_run();
        let mut legacy = serde_json::to_value(SavedState::default()).unwrap();
        legacy["scanHistory"] = serde_json::json!([{ "kind": "scan", "run": &run }]);
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let mut saved = load(&path).unwrap();
        assert_eq!(saved.recent_history()[0].1.id, run.id);
        save(&path, &saved).unwrap();
        let archive = history_path(&path, saved.scan_history[0].file_id.unwrap());
        let contents = fs::read(&archive).unwrap();
        let state: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(state.get("scanHistory").is_none());
        assert_eq!(state["scanHistoryFiles"].as_array().unwrap().len(), 1);

        // A save must leave the existing archive file itself in place.
        let file = fs::OpenOptions::new().write(true).open(&archive).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
            .unwrap();
        let modified = file.metadata().unwrap().modified().unwrap();
        saved.config.max_tcp_ms = Some(234.0);
        save(&path, &saved).unwrap();
        assert_eq!(
            fs::metadata(&archive).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(fs::read(&archive).unwrap(), contents);
        let reopened = load(&path).unwrap();
        assert_eq!(reopened.config.max_tcp_ms, Some(234.0));
        assert_eq!(reopened.recent_history()[0].1.id, run.id);
    }

    #[test]
    fn an_unavailable_archive_preserves_other_state_and_its_reference() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut saved = SavedState::default();
        saved.archive_run(HistoryKind::Scan, legacy_run());
        save(&path, &saved).unwrap();
        let archive = history_path(&path, saved.scan_history[0].file_id.unwrap());
        let bytes = fs::read(&archive).unwrap();
        fs::remove_file(&archive).unwrap();

        let mut reopened = load_for_startup(&path).unwrap();
        assert!(reopened
            .warning
            .as_deref()
            .unwrap()
            .contains("测速归档无法读取"));
        assert_eq!(reopened.unavailable_history_files.len(), 1);
        assert!(reopened.recent_history().is_empty());
        reopened.config.max_tcp_ms = Some(240.0);
        save(&path, &reopened).unwrap();
        fs::write(&archive, bytes).unwrap();
        let recovered = load(&path).unwrap();
        assert_eq!(recovered.config.max_tcp_ms, Some(240.0));
        assert_eq!(recovered.recent_history().len(), 1);
        assert!(recovered.unavailable_history_files.is_empty());
    }

    #[test]
    fn archive_cleanup_follows_the_successful_state_commit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut saved = SavedState::default();
        saved.archive_run(HistoryKind::Scan, legacy_run());
        save(&path, &saved).unwrap();
        let archive = history_path(&path, saved.scan_history[0].file_id.unwrap());

        let blocked_path = dir.path().join("blocked.json");
        fs::create_dir(&blocked_path).unwrap();
        assert!(save(&blocked_path, &saved).is_err());
        assert!(!history_path(&blocked_path, saved.scan_history[0].file_id.unwrap()).exists());
        assert!(archive.is_file());

        let next = SavedState::default();
        save_replacing_history(&path, &next, &saved.scan_history).unwrap();
        assert!(!archive.exists());
        assert!(load(&path).unwrap().scan_history.is_empty());
    }

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
    fn startup_restores_a_newer_checkpoint_and_persists_the_cancelled_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut run = legacy_run();
        run.status = "running".into();
        run.finished_at = None;
        let saved = SavedState {
            last_run: Some(run.clone()),
            measurement_revision: 2,
            ..Default::default()
        };
        save(&path, &saved).unwrap();
        let original = fs::read(&path).unwrap();
        run.results[0].tcp_ms = 42.0;
        let attempts = run.config.stability_attempts.unwrap();
        run.results[1].tcp_stability = Some(crate::stability::summarize(
            attempts,
            run.config.max_tcp_ms.unwrap(),
            vec![Some(42.0); attempts],
        ));
        run.results[1].measurement_passed = true;
        run.results[1].eligible = false;
        run.progress.downloaded_bytes = 2048;
        save_checkpoint(&path, &run, 3).unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);

        let loaded = load_for_startup(&path).unwrap();
        let recovered = loaded.last_run.unwrap();
        assert_eq!(recovered.status, "cancelled");
        assert_eq!(recovered.results[0].tcp_ms, 42.0);
        assert!(recovered.results[1].measurement_passed);
        assert!(recovered.results[1].eligible);
        assert_eq!(recovered.results[1].role.as_deref(), Some("candidate"));
        assert_eq!(recovered.progress.downloaded_bytes, 2048);
        assert!(!checkpoint_path(&path).exists());
        let reopened = load_for_startup(&path).unwrap().last_run.unwrap();
        assert_eq!(reopened.finished_at, recovered.finished_at);
        assert_eq!(reopened.results[0].tcp_ms, 42.0);
    }

    #[test]
    fn startup_ignores_old_revisions_other_runs_and_corrupt_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut run = legacy_run();
        run.status = "running".into();
        run.results[0].tcp_ms = 22.0;
        let mut saved = SavedState {
            last_run: Some(run.clone()),
            measurement_revision: 10,
            ..Default::default()
        };
        save(&path, &saved).unwrap();
        run.results[0].tcp_ms = 99.0;
        save_checkpoint(&path, &run, 9).unwrap();
        assert_eq!(
            load(&path).unwrap().last_run.unwrap().results[0].tcp_ms,
            22.0
        );

        run.id = "old-id".into();
        save_checkpoint(&path, &run, 11).unwrap();
        assert_eq!(
            load(&path).unwrap().last_run.unwrap().results[0].tcp_ms,
            22.0
        );

        fs::write(checkpoint_path(&path), b"invalid checkpoint").unwrap();
        let recovered = load_for_startup(&path).unwrap();
        assert_eq!(recovered.last_run.unwrap().results[0].tcp_ms, 22.0);
        assert!(recovered.warning.unwrap().contains("已使用主状态文件"));
        assert!(recovery_backups(&path).is_empty());

        let durable = saved.last_run.as_mut().unwrap();
        durable.status = "completed".into();
        run.id = durable.id.clone();
        save(&path, &saved).unwrap();
        save_checkpoint(&path, &run, 11).unwrap();
        let reopened = load(&path).unwrap().last_run.unwrap();
        assert_eq!(reopened.status, "completed");
        assert_eq!(reopened.results[0].tcp_ms, 22.0);
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

    #[test]
    fn startup_does_not_publish_a_recovery_backup_without_its_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let original = b"corrupt state";
        let source = b"192.0.2.1\n# recovery evidence";
        let live = favorite_source_path(&path, Uuid::new_v4());
        write_atomic(&live, source).unwrap();
        fs::write(
            path.with_extension("recovery-favorite-sources"),
            b"occupied",
        )
        .unwrap();
        fs::write(&path, original).unwrap();

        assert!(load_for_startup(&path).unwrap_err().contains("已停止启动"));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read(live).unwrap(), source);
        assert!(recovery_backups(&path).is_empty());
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
            update_revision: 0,
            quick_validation: Some(false),
            retest_ip: None,
            download_only: false,
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
    fn single_ip_retest_keeps_valid_measurements_outside_the_original_filter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut run = legacy_run();
        let mut result = run.results.swap_remove(1);
        result.tcp_ms = 40.0;
        result.tcp_stability = Some(crate::stability::summarize(
            100,
            200.0,
            vec![Some(45.0); 100],
        ));
        result.measurement_passed = true;
        assert_eq!(result.region_code, "JP");
        assert!(result.has_complete_measurement(&run.config));
        run.retest_ip = Some(result.ip.clone());
        run.config.regions = vec!["HK".into()];
        run.results = vec![result];
        save(
            &path,
            &SavedState {
                last_run: Some(run),
                ..Default::default()
            },
        )
        .unwrap();

        let restored = load(&path).unwrap().last_run.unwrap();
        assert_eq!(restored.config.regions, ["HK", "JP"]);
        assert!(restored.results[0].measurement_passed);
        assert!(restored.results[0].eligible);
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
    fn legacy_favorites_share_their_exact_candidate_list_and_survive_an_older_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut saved = SavedState::default();
        let mut run = legacy_run();
        let source_id = Uuid::new_v4();
        run.id = source_id.to_string();
        run.config.source = "custom".into();
        let source = format!(
            "{}\n# shared-source-marker{}",
            run.results[0].ip,
            " ".repeat(256_000)
        );
        for folder in saved.favorites.folders.clone().into_iter().take(2) {
            saved
                .favorites
                .save_snapshot(&folder.id, &run, &run.results[0].ip, "")
                .unwrap();
        }
        let mut legacy = serde_json::to_value(&saved).unwrap();
        for entry in legacy["favorites"]["entries"].as_array_mut().unwrap() {
            entry.as_object_mut().unwrap().remove("candidateListId");
            entry["config"]["customIps"] = serde_json::json!(source);
        }
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.favorites.candidate_lists.len(), 1);
        for entry in &loaded.favorites.entries {
            let id = entry.candidate_list_id.as_deref().unwrap();
            assert_eq!(&*loaded.favorites.candidate_lists[id], source);
            assert!(entry.config.custom_ips.is_empty());
        }
        save(&path, &loaded).unwrap();
        let disk = fs::read_to_string(&path).unwrap();
        assert_eq!(disk.matches("shared-source-marker").count(), 1);
        let recovery = favorite_source_path(&path, source_id);
        assert_eq!(fs::read_to_string(&recovery).unwrap(), source);
        fs::remove_file(&recovery).unwrap();
        load_for_startup(&path).unwrap();
        assert_eq!(fs::read_to_string(&recovery).unwrap(), source);

        // An older writer preserves the favorites and run IDs while stripping
        // the newer fields. The archived scan itself is no longer present.
        let mut older: serde_json::Value = serde_json::from_str(&disk).unwrap();
        older["favorites"]
            .as_object_mut()
            .unwrap()
            .remove("candidateLists");
        for entry in older["favorites"]["entries"].as_array_mut().unwrap() {
            entry.as_object_mut().unwrap().remove("candidateListId");
        }
        older["config"]["downloadSeconds"] = serde_json::json!(5);
        fs::write(&path, serde_json::to_vec(&older).unwrap()).unwrap();
        let mut reopened = load(&path).unwrap();
        assert_eq!(reopened.config.download_seconds, 5);
        assert!(reopened
            .favorites
            .entries
            .iter()
            .all(|entry| entry.candidate_list_id.as_deref() == Some(run.id.as_str())));
        let previous = reopened.clone();
        let entry_id = reopened.favorites.entries[0].id.clone();
        reopened.favorites.delete_entry(&entry_id).unwrap();
        assert_eq!(&*reopened.favorites.candidate_lists[&run.id], source);
        save_favorite_changes(&path, &reopened, &previous).unwrap();
        assert!(recovery.is_file());
        let previous = reopened.clone();
        let entry_id = reopened.favorites.entries[0].id.clone();
        reopened.favorites.delete_entry(&entry_id).unwrap();
        assert!(reopened.favorites.candidate_lists.is_empty());
        save_favorite_changes(&path, &reopened, &previous).unwrap();
        assert!(!recovery.exists());
    }

    #[test]
    fn saving_defaults_after_recovery_keeps_the_source_needed_by_the_original_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut saved = SavedState::default();
        let mut run = legacy_run();
        let source_id = Uuid::new_v4();
        run.id = source_id.to_string();
        run.config.source = "custom".into();
        let source = format!("{}\n# sole copy", run.results[0].ip);
        run.config.custom_ips = source.clone();
        let folder = saved.favorites.folders[0].id.clone();
        saved
            .favorites
            .save_snapshot(&folder, &run, &run.results[0].ip, "")
            .unwrap();
        save(&path, &saved).unwrap();

        let mut older: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        older["favorites"]
            .as_object_mut()
            .unwrap()
            .remove("candidateLists");
        older["favorites"]["entries"][0]
            .as_object_mut()
            .unwrap()
            .remove("candidateListId");
        older["config"]["downloadSeconds"] = serde_json::json!("invalid");
        fs::write(&path, serde_json::to_vec(&older).unwrap()).unwrap();
        let fallback = load_for_startup(&path).unwrap();
        assert!(fallback.warning.is_some());
        assert!(fallback.favorites.entries.is_empty());
        let mut runtime = crate::RuntimeState {
            saved: fallback,
            cancellation: None,
            worker: None,
        };
        runtime.interrupt_for_shutdown(&path).unwrap();
        let unrelated = runtime.saved.clone();
        runtime
            .saved
            .favorites
            .create_folder("另一个收藏夹")
            .unwrap();
        save_favorite_changes(&path, &runtime.saved, &unrelated).unwrap();
        assert_eq!(
            fs::read_to_string(favorite_source_path(&path, source_id)).unwrap(),
            source
        );

        let backup = recovery_backups(&path).pop().unwrap();
        assert!(backup
            .to_string_lossy()
            .ends_with(&format!(".{RECOVERY_WITH_SOURCES_EXTENSION}")));
        assert_eq!(
            fs::read_to_string(recovery_favorite_source_path(&path, source_id)).unwrap(),
            source
        );
        let mut repaired: serde_json::Value =
            serde_json::from_slice(&fs::read(backup).unwrap()).unwrap();
        repaired["config"]["downloadSeconds"] = serde_json::json!(3);
        let repaired = serde_json::to_vec(&repaired).unwrap();
        fs::write(&path, &repaired).unwrap();
        let reopened = load(&path).unwrap();
        assert_eq!(
            reopened.favorites.entries[0].candidate_list_id.as_deref(),
            Some(run.id.as_str())
        );
        assert_eq!(&*reopened.favorites.candidate_lists[&run.id], source);

        let previous = reopened;
        let mut next = previous.clone();
        let mut replacement = run.clone();
        replacement.id = Uuid::new_v4().to_string();
        replacement.config = ScanConfig::default();
        next.favorites
            .save_snapshot(&folder, &replacement, &run.results[0].ip, "更新")
            .unwrap();
        save_favorite_changes(&path, &next, &previous).unwrap();
        assert!(!favorite_source_path(&path, source_id).exists());
        fs::write(&path, &repaired).unwrap();
        let reopened_again = load(&path).unwrap();
        assert_eq!(
            reopened_again
                .favorites
                .candidate_lists
                .get(&run.id)
                .map(|list| &**list),
            Some(source.as_str())
        );
    }

    #[test]
    fn retiring_a_source_keeps_legacy_recovery_data_even_when_archiving_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut saved = SavedState::default();
        let mut run = legacy_run();
        let id = Uuid::new_v4();
        run.id = id.to_string();
        run.config.source = "custom".into();
        run.config.custom_ips = format!("{}\n# original list", run.results[0].ip);
        let folder = saved.favorites.folders[0].id.clone();
        saved
            .favorites
            .save_snapshot(&folder, &run, &run.results[0].ip, "")
            .unwrap();
        save(&path, &saved).unwrap();
        let legacy = dir.path().join("state.json.recovery-old.bak");
        fs::write(&legacy, b"unreadable legacy backup").unwrap();
        let recovery_dir = path.with_extension("recovery-favorite-sources");
        fs::write(&recovery_dir, b"occupied").unwrap();
        let live = favorite_source_path(&path, id);
        fs::remove_file(&live).unwrap();
        let mut next = saved.clone();
        let favorite = next.favorites.entries[0].id.clone();
        next.favorites.delete_entry(&favorite).unwrap();
        let original = fs::read(&path).unwrap();
        fs::create_dir(&live).unwrap();
        assert!(save_favorite_changes(&path, &next, &saved).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::remove_dir(&live).unwrap();
        fs::write(&live, b"truncated copy").unwrap();
        save_favorite_changes(&path, &next, &saved).unwrap();
        assert_eq!(fs::read_to_string(&live).unwrap(), run.config.custom_ips);
        assert!(load(&path).unwrap().favorites.entries.is_empty());

        fs::remove_file(recovery_dir).unwrap();
        fs::write(
            legacy,
            serde_json::to_vec(&serde_json::json!({
                "config": {"downloadSeconds": "invalid"},
                "favorites": {"entries": [{"runId": id}]}
            }))
            .unwrap(),
        )
        .unwrap();
        save(&path, &saved).unwrap();
        fs::remove_file(&live).unwrap();
        save_favorite_changes(&path, &next, &saved).unwrap();
        assert!(!live.exists());
        assert_eq!(
            fs::read_to_string(recovery_favorite_source_path(&path, id)).unwrap(),
            run.config.custom_ips
        );
    }

    #[test]
    fn legacy_backups_only_reference_candidate_sources_missing_from_their_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let [implicit, explicit, inline, empty, legacy_empty, embedded, other_run, completed] =
            [(); 8].map(|_| Uuid::new_v4());
        let legacy = dir.path().join("state.json.recovery-legacy.bak");
        let backup = serde_json::json!({
            "config": { "downloadSeconds": "invalid" },
            "favorites": {
                "candidateLists": { (embedded.to_string()): "192.0.2.1/24" },
                "entries": [
                    { "runId": implicit, "config": { "customIps": "" } },
                    { "runId": other_run, "candidateListId": explicit, "config": { "customIps": "" } },
                    { "runId": inline, "config": { "customIps": "192.0.2.1/24" } },
                    { "runId": empty, "candidateListId": null, "candidateListKnownEmpty": true, "config": { "customIps": "" } },
                    { "runId": legacy_empty, "candidateListId": null, "config": { "customIps": "" } },
                    { "runId": embedded, "candidateListId": embedded, "config": { "customIps": "" } }
                ]
            }
        });
        fs::write(legacy, serde_json::to_vec(&backup).unwrap()).unwrap();
        let marked = dir.path().join(format!(
            "state.json.recovery-new.{RECOVERY_WITH_SOURCES_EXTENSION}"
        ));
        fs::write(
            marked,
            serde_json::to_vec(
                &serde_json::json!({"favorites": {"entries": [{"runId": completed}]}}),
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            legacy_recovery_source_ids(&path),
            Some(HashSet::from([implicit, explicit, legacy_empty]))
        );
        fs::write(
            dir.path().join("state.json.recovery-unreadable.bak"),
            b"broken JSON",
        )
        .unwrap();
        assert_eq!(legacy_recovery_source_ids(&path), None);
    }

    #[test]
    fn older_writer_after_restoring_an_embedded_source_can_recover_it_from_the_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let backup_path = dir.path().join("state.json.recovery-old.bak");
        let mut saved = SavedState::default();
        let mut run = legacy_run();
        let id = Uuid::new_v4();
        run.id = id.to_string();
        run.config.source = "custom".into();
        let source = format!("{}\n# original source", run.results[0].ip);
        run.config.custom_ips = source.clone();
        let folder = saved.favorites.folders[0].id.clone();
        saved
            .favorites
            .save_snapshot(&folder, &run, &run.results[0].ip, "")
            .unwrap();
        save(&path, &saved).unwrap();
        let mut backup: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        backup["config"]["downloadSeconds"] = serde_json::json!("invalid");
        let backup_bytes = serde_json::to_vec(&backup).unwrap();
        fs::write(&backup_path, &backup_bytes).unwrap();

        let mut next = saved.clone();
        let favorite = next.favorites.entries[0].id.clone();
        next.favorites.delete_entry(&favorite).unwrap();
        save_favorite_changes(&path, &next, &saved).unwrap();
        let live = favorite_source_path(&path, id);
        assert!(!live.exists());
        assert!(!recovery_favorite_source_path(&path, id).exists());

        // Restore the backup and model an older writer that drops the new
        // fields while keeping the original favorite and its run ID.
        let mut older_writeback = backup.clone();
        older_writeback["config"]["downloadSeconds"] = serde_json::json!(5);
        older_writeback["favorites"]
            .as_object_mut()
            .unwrap()
            .remove("candidateLists");
        older_writeback["favorites"]["entries"][0]
            .as_object_mut()
            .unwrap()
            .remove("candidateListId");
        let older_writeback = serde_json::to_vec(&older_writeback).unwrap();
        fs::write(&path, &older_writeback).unwrap();
        fs::write(
            dir.path().join("state.json.recovery-broken.bak"),
            b"broken JSON",
        )
        .unwrap();

        let conflicting_path = dir.path().join("state.json.recovery-conflict.bak");
        let mut conflicting = backup;
        conflicting["favorites"]["candidateLists"][&run.id] = serde_json::json!("192.0.2.9/24");
        fs::write(&conflicting_path, serde_json::to_vec(&conflicting).unwrap()).unwrap();
        assert!(load(&path).unwrap().favorites.candidate_lists.is_empty());
        fs::remove_file(conflicting_path).unwrap();

        let recovered = load_for_startup(&path).unwrap();
        assert_eq!(recovered.config.download_seconds, 5);
        assert_eq!(
            recovered
                .favorites
                .candidate_lists
                .get(&run.id)
                .map(Arc::as_ref),
            Some(source.as_str())
        );
        assert_eq!(fs::read_to_string(live).unwrap(), source);
        assert_eq!(fs::read(&backup_path).unwrap(), backup_bytes);
        assert!(!recovery_favorite_source_path(&path, id).exists());

        fs::remove_file(backup_path).unwrap();
        fs::write(&path, older_writeback).unwrap();
        assert_eq!(
            load(&path)
                .unwrap()
                .favorites
                .candidate_lists
                .get(&run.id)
                .map(Arc::as_ref),
            Some(source.as_str())
        );
    }

    #[test]
    fn empty_favorite_source_files_fall_back_and_are_replaced_on_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut saved = SavedState::default();
        let mut run = legacy_run();
        let id = Uuid::new_v4();
        run.id = id.to_string();
        run.config.source = "custom".into();
        let source = format!("{}\n# complete source", run.results[0].ip);
        run.config.custom_ips = source.clone();
        let folder = saved.favorites.folders[0].id.clone();
        saved
            .favorites
            .save_snapshot(&folder, &run, &run.results[0].ip, "")
            .unwrap();
        save(&path, &saved).unwrap();
        let original = fs::read(&path).unwrap();
        let backup = dir.path().join("state.json.recovery-old.bak");
        fs::write(&backup, &original).unwrap();

        let mut empty_index: serde_json::Value = serde_json::from_slice(&original).unwrap();
        empty_index["favorites"]["candidateLists"][&run.id] = serde_json::json!("");
        let empty_index = serde_json::to_vec(&empty_index).unwrap();
        let live = favorite_source_path(&path, id);
        let archived = recovery_favorite_source_path(&path, id);
        for archive_contents in [source.as_bytes(), &[][..]] {
            fs::write(&path, &empty_index).unwrap();
            fs::write(&live, b"").unwrap();
            write_atomic(&archived, archive_contents).unwrap();
            let reopened = load_for_startup(&path).unwrap();
            assert_eq!(
                reopened
                    .favorites
                    .candidate_lists
                    .get(&run.id)
                    .map(Arc::as_ref),
                Some(source.as_str())
            );
            assert_eq!(fs::read_to_string(&live).unwrap(), source);
        }
        assert_eq!(fs::read(&backup).unwrap(), original);

        archive_favorite_source(&path, id, None).unwrap();
        assert_eq!(fs::read_to_string(archived).unwrap(), source);
    }

    #[test]
    fn startup_repairs_a_damaged_sidecar_before_an_older_writer_drops_the_embedded_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut saved = SavedState::default();
        let mut run = legacy_run();
        let id = Uuid::new_v4();
        run.id = id.to_string();
        run.config.source = "custom".into();
        let source = format!("{}\n# complete source", run.results[0].ip);
        run.config.custom_ips = source.clone();
        let folder = saved.favorites.folders[0].id.clone();
        saved
            .favorites
            .save_snapshot(&folder, &run, &run.results[0].ip, "")
            .unwrap();
        save(&path, &saved).unwrap();
        let original = fs::read(&path).unwrap();
        let mut older: serde_json::Value = serde_json::from_slice(&original).unwrap();
        older["favorites"]
            .as_object_mut()
            .unwrap()
            .remove("candidateLists");
        older["favorites"]["entries"][0]
            .as_object_mut()
            .unwrap()
            .remove("candidateListId");
        let older = serde_json::to_vec(&older).unwrap();
        let live = favorite_source_path(&path, id);

        for damage in [&b"192.0.2.1"[..], &vec![0xff; source.len()]] {
            fs::write(&path, &original).unwrap();
            fs::write(&live, damage).unwrap();
            let reopened = load_for_startup(&path).unwrap();
            assert!(reopened.warning.is_none());
            fs::write(&path, &older).unwrap();
            let downgraded = load(&path).unwrap();
            assert_eq!(
                downgraded
                    .favorites
                    .candidate_lists
                    .get(&run.id)
                    .map(Arc::as_ref),
                Some(source.as_str())
            );
        }
    }

    #[test]
    fn independent_source_takes_priority_when_legacy_run_metadata_is_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut saved = SavedState::default();
        let mut run = legacy_run();
        run.id = Uuid::new_v4().to_string();
        let ip = run.results[0].ip.clone();
        let source = format!("{ip}\n# retained custom list");
        run.config.custom_ips = source.clone();
        let folder = saved.favorites.folders[0].id.clone();
        saved
            .favorites
            .save_snapshot(&folder, &run, &ip, "")
            .unwrap();
        let run_id = run.id.clone();
        saved.last_run = Some(run);
        save(&path, &saved).unwrap();
        let mut older: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        older["favorites"]
            .as_object_mut()
            .unwrap()
            .remove("candidateLists");
        older["favorites"]["entries"][0]
            .as_object_mut()
            .unwrap()
            .remove("candidateListId");

        older["lastRun"]["config"]
            .as_object_mut()
            .unwrap()
            .remove("customIps");
        fs::write(&path, serde_json::to_vec(&older).unwrap()).unwrap();
        let recovered = load(&path).unwrap();
        assert_eq!(
            recovered
                .favorites
                .candidate_lists
                .get(&run_id)
                .map(Arc::as_ref),
            Some(source.as_str())
        );

        older["lastRun"]["config"]["customIps"] = serde_json::json!("");
        fs::write(&path, serde_json::to_vec(&older).unwrap()).unwrap();
        let recovered = load(&path).unwrap();
        assert_eq!(
            recovered
                .favorites
                .candidate_lists
                .get(&run_id)
                .map(Arc::as_ref),
            Some(source.as_str())
        );

        older["favorites"]["entries"][0]["candidateListId"] = serde_json::Value::Null;
        fs::write(&path, serde_json::to_vec(&older).unwrap()).unwrap();
        let repaired = load(&path).unwrap();
        assert_eq!(
            repaired.favorites.entries[0].candidate_list_id.as_deref(),
            Some(run_id.as_str())
        );
        assert_eq!(
            repaired
                .favorites
                .candidate_lists
                .get(&run_id)
                .map(Arc::as_ref),
            Some(source.as_str())
        );
    }

    #[test]
    fn missing_source_metadata_remains_unknown_after_saving_the_state_again() {
        for archived in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.json");
            let mut saved = SavedState::default();
            let mut run = legacy_run();
            let id = Uuid::new_v4();
            run.id = id.to_string();
            let ip = run.results[0].ip.clone();
            let source = format!("{ip}\n# recoverable custom list");
            run.config.custom_ips = source.clone();
            let folder = saved.favorites.folders[0].id.clone();
            saved
                .favorites
                .save_snapshot(&folder, &run, &ip, "")
                .unwrap();
            if archived {
                saved.archive_run(HistoryKind::Scan, run);
            } else {
                saved.last_run = Some(run);
            }
            save(&path, &saved).unwrap();
            let complete = fs::read(&path).unwrap();
            let mut older: serde_json::Value = serde_json::from_slice(&complete).unwrap();
            older["favorites"]
                .as_object_mut()
                .unwrap()
                .remove("candidateLists");
            older["favorites"]["entries"][0]
                .as_object_mut()
                .unwrap()
                .remove("candidateListId");
            if archived {
                let archive = history_path(&path, saved.scan_history[0].file_id.unwrap());
                let mut history: serde_json::Value =
                    serde_json::from_slice(&fs::read(&archive).unwrap()).unwrap();
                history["run"]["config"]
                    .as_object_mut()
                    .unwrap()
                    .remove("customIps");
                fs::write(archive, serde_json::to_vec(&history).unwrap()).unwrap();
            } else {
                older["lastRun"]["config"]
                    .as_object_mut()
                    .unwrap()
                    .remove("customIps");
            }
            fs::write(&path, serde_json::to_vec(&older).unwrap()).unwrap();
            fs::remove_file(favorite_source_path(&path, id)).unwrap();

            let mut reopened = load(&path).unwrap();
            assert!(reopened.favorites.entries[0].candidate_list_id.is_unknown());
            reopened.config.download_seconds = 5;
            save(&path, &reopened).unwrap();
            fs::write(dir.path().join("state.json.recovery-old.bak"), complete).unwrap();

            let recovered = load_for_startup(&path).unwrap();
            assert_eq!(recovered.config.download_seconds, 5);
            assert_eq!(
                recovered
                    .favorites
                    .candidate_lists
                    .get(&id.to_string())
                    .map(Arc::as_ref),
                Some(source.as_str())
            );
            assert_eq!(
                fs::read_to_string(favorite_source_path(&path, id)).unwrap(),
                source
            );
        }
    }

    #[test]
    fn previously_saved_empty_marker_can_recover_a_backup_only_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let backup = dir.path().join("state.json.recovery-old.bak");
        let mut saved = SavedState::default();
        let mut run = legacy_run();
        let id = Uuid::new_v4();
        run.id = id.to_string();
        let ip = run.results[0].ip.clone();
        let source = format!("{ip}\n# preserved only in the original backup");
        run.config.custom_ips = source.clone();
        let folder = saved.favorites.folders[0].id.clone();
        saved
            .favorites
            .save_snapshot(&folder, &run, &ip, "原备注")
            .unwrap();
        saved.last_run = Some(run);
        save(&path, &saved).unwrap();
        let complete = fs::read(&path).unwrap();
        fs::write(&backup, &complete).unwrap();
        let previous = saved.clone();
        saved
            .favorites
            .delete_entry(&previous.favorites.entries[0].id)
            .unwrap();
        save_favorite_changes(&path, &saved, &previous).unwrap();
        let live = favorite_source_path(&path, id);
        assert!(!live.exists());
        assert!(!recovery_favorite_source_path(&path, id).exists());

        let mut mistaken: serde_json::Value = serde_json::from_slice(&complete).unwrap();
        mistaken["favorites"]
            .as_object_mut()
            .unwrap()
            .remove("candidateLists");
        mistaken["favorites"]["entries"][0]["candidateListId"] = serde_json::Value::Null;
        mistaken["lastRun"]["config"]["customIps"] = serde_json::json!("");
        fs::write(&path, serde_json::to_vec(&mistaken).unwrap()).unwrap();

        // Saving while the backup is unavailable must not turn the legacy
        // null into a confirmation that there was no candidate list.
        fs::remove_file(&backup).unwrap();
        let mut unresolved = load(&path).unwrap();
        assert!(!unresolved.favorites.entries[0].candidate_list_known_empty);
        let another_folder = unresolved.favorites.folders[1].id.clone();
        for folder in [&folder, &another_folder] {
            unresolved
                .favorites
                .save_snapshot(
                    folder,
                    unresolved.last_run.as_ref().unwrap(),
                    &ip,
                    "暂时没有备份",
                )
                .unwrap();
        }
        assert!(unresolved
            .favorites
            .entries
            .iter()
            .all(|entry| !entry.candidate_list_known_empty));
        save(&path, &unresolved).unwrap();
        fs::write(&backup, &complete).unwrap();
        let mut recovered = load_for_startup(&path).unwrap();
        assert_eq!(
            recovered.favorites.entries[0].candidate_list_id.as_deref(),
            Some(id.to_string().as_str())
        );
        assert_eq!(
            recovered.favorites.entries[1].candidate_list_id.as_deref(),
            recovered.favorites.entries[0].candidate_list_id.as_deref()
        );
        assert_eq!(
            recovered
                .favorites
                .candidate_lists
                .get(&id.to_string())
                .map(Arc::as_ref),
            Some(source.as_str())
        );
        assert_eq!(fs::read_to_string(&live).unwrap(), source);
        assert_eq!(fs::read(&backup).unwrap(), complete);

        let previous = recovered.clone();
        recovered
            .favorites
            .save_snapshot(
                &folder,
                recovered.last_run.as_ref().unwrap(),
                &ip,
                "更新备注",
            )
            .unwrap();
        save_favorite_changes(&path, &recovered, &previous).unwrap();
        assert_eq!(
            recovered.favorites.entries[0].candidate_list_id.as_deref(),
            Some(id.to_string().as_str())
        );
        assert_eq!(fs::read_to_string(live).unwrap(), source);
    }

    #[test]
    fn known_empty_favorite_sources_ignore_recovery_backups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut saved = SavedState::default();
        let mut run = legacy_run();
        let id = Uuid::new_v4();
        run.id = id.to_string();
        let folder = saved.favorites.folders[0].id.clone();
        saved
            .favorites
            .save_snapshot(&folder, &run, &run.results[0].ip, "")
            .unwrap();
        save(&path, &saved).unwrap();
        let backup = serde_json::json!({"favorites": {"candidateLists": {(run.id.clone()): "192.0.2.1/24"}}});
        fs::write(
            dir.path().join("state.json.recovery-stale.bak"),
            serde_json::to_vec(&backup).unwrap(),
        )
        .unwrap();
        assert!(load(&path).unwrap().favorites.candidate_lists.is_empty());
        let current: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            current["favorites"]["entries"][0].get("candidateListId"),
            Some(&serde_json::Value::Null)
        );
        assert_eq!(
            current["favorites"]["entries"][0]["candidateListKnownEmpty"],
            true
        );

        // Older writers omit the marker. A matching archived scan still
        // proves that this favorite had no separate source to recover.
        saved.archive_run(HistoryKind::Scan, run);
        save(&path, &saved).unwrap();
        let mut older: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        older["favorites"]["entries"][0]
            .as_object_mut()
            .unwrap()
            .remove("candidateListId");
        older["favorites"]["entries"][0]
            .as_object_mut()
            .unwrap()
            .remove("candidateListKnownEmpty");
        fs::write(&path, serde_json::to_vec(&older).unwrap()).unwrap();
        let reopened = load(&path).unwrap();
        assert!(reopened.favorites.candidate_lists.is_empty());
        assert!(reopened.favorites.entries[0].candidate_list_known_empty);
        assert_eq!(
            serde_json::to_value(&reopened.favorites.entries[0])
                .unwrap()
                .get("candidateListId"),
            Some(&serde_json::Value::Null)
        );
    }

    #[test]
    fn unrelated_legacy_backup_does_not_archive_or_block_removal_of_later_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(
            dir.path().join("state.json.recovery-old.bak"),
            br#"{"config":{"downloadSeconds":"invalid"},"favorites":{"entries":[]}}"#,
        )
        .unwrap();

        for block_source_writes in [false, true] {
            let mut saved = SavedState::default();
            let mut run = legacy_run();
            let id = Uuid::new_v4();
            run.id = id.to_string();
            run.config.source = "custom".into();
            run.config.custom_ips = format!("{}\n# later list", run.results[0].ip);
            let folder = saved.favorites.folders[0].id.clone();
            saved
                .favorites
                .save_snapshot(&folder, &run, &run.results[0].ip, "")
                .unwrap();
            save(&path, &saved).unwrap();

            let live = favorite_source_path(&path, id);
            if block_source_writes {
                fs::remove_file(&live).unwrap();
                fs::create_dir(&live).unwrap();
                fs::write(
                    path.with_extension("recovery-favorite-sources"),
                    b"occupied",
                )
                .unwrap();
            }
            let mut next = saved.clone();
            let favorite = next.favorites.entries[0].id.clone();
            next.favorites.delete_entry(&favorite).unwrap();
            save_favorite_changes(&path, &next, &saved).unwrap();
            assert!(load(&path).unwrap().favorites.entries.is_empty());
            assert!(!live.is_file());
            assert!(!recovery_favorite_source_path(&path, id).exists());
        }
    }

    #[test]
    fn independent_recovery_backup_does_not_retain_later_unrelated_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, b"corrupt state without favorites").unwrap();
        let mut saved = load_for_startup(&path).unwrap();
        let mut run = legacy_run();
        let id = Uuid::new_v4();
        run.id = id.to_string();
        run.config.source = "custom".into();
        run.config.custom_ips = format!("{}\n# new list", run.results[0].ip);
        let folder = saved.favorites.folders[0].id.clone();
        saved
            .favorites
            .save_snapshot(&folder, &run, &run.results[0].ip, "")
            .unwrap();
        save(&path, &saved).unwrap();
        let mut next = saved.clone();
        let favorite = next.favorites.entries[0].id.clone();
        next.favorites.delete_entry(&favorite).unwrap();
        save_favorite_changes(&path, &next, &saved).unwrap();
        assert!(!favorite_source_path(&path, id).exists());
        assert!(!recovery_favorite_source_path(&path, id).exists());
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
        let (job, _) = saved
            .favorites
            .prepare_test(pending, Some(&folder), None, None, None)
            .unwrap();
        let mut checkpoint = job.run.clone();
        checkpoint.results[0].tcp_ms = 42.0;
        checkpoint.results[0].region_code = "GB".into();
        saved.favorite_test = Some(job);
        saved.last_run = Some(previous);
        saved.favorites.revision = 13;
        save(&path, &saved).unwrap();
        save_checkpoint(&path, &checkpoint, 1).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.favorites.revision, 14);
        assert!(!checkpoint_path(&path).exists());
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
        assert_eq!(
            loaded.favorites.entries[0].result.exclusion_code,
            Some(crate::model::ExclusionCode::Cancelled)
        );
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
            let state = SavedState {
                candidate_coverage_version: 0,
                config: ScanConfig {
                    source: source.into(),
                    max_candidates: cap,
                    ..Default::default()
                },
                ..Default::default()
            };
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
            baselines: Vec::new(),
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
