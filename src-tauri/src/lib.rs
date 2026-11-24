use serde::{Deserialize, Serialize};
use std::{
    backtrace::Backtrace,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter};

const DUPLICATE_DIR_NAME: &str = "重复文件";
const PROGRESS_EVENT: &str = "merge-progress";
const SEARCH_PROGRESS_EVENT: &str = "search-progress";
const MAX_SEARCH_RESULTS: usize = 10_000;

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct MergeReport {
    copied_files: usize,
    moved_files: usize,
    created_dirs: usize,
    merged_dirs: usize,
    duplicate_files: usize,
    duplicate_size_bytes: u64,
    duplicate_entries: Vec<DuplicateEntry>,
    path_conflicts: usize,
    skipped_count: usize,
    skipped_entries: Vec<String>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct DuplicateEntry {
    path_segments: Vec<String>,
    size_bytes: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MergeError {
    message: String,
    details: String,
    backtrace: String,
}

impl MergeError {
    fn new(message: impl Into<String>, details: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            details: details.into(),
            backtrace: Backtrace::force_capture().to_string(),
        }
    }

    fn from_io(fallback_message: &str, error: io::Error) -> Self {
        let details = error.to_string();
        let message = if is_storage_full_error(&error, &details) {
            "磁盘空间不足，无法继续处理。请清理目标磁盘空间后重试。"
        } else {
            fallback_message
        };

        Self::new(message, details)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MergeProgress {
    total: usize,
    completed: usize,
    current_path: String,
    stage: ProgressStage,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
enum ProgressStage {
    Scanning,
    Merging,
    Finished,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum OperationMode {
    Copy,
    Move,
}

#[derive(Clone, Copy, Debug)]
struct MergeOptions {
    operation_mode: OperationMode,
    archive_duplicates: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchResult {
    name: String,
    path: String,
    parent_path: String,
    is_dir: bool,
    size_bytes: u64,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchReport {
    results: Vec<SearchResult>,
    scanned_entries: usize,
    skipped_count: usize,
    truncated: bool,
    cancelled: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchProgress {
    total: usize,
    scanned_entries: usize,
    matched_entries: usize,
    skipped_count: usize,
    current_path: String,
    stage: SearchProgressStage,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
enum SearchProgressStage {
    Counting,
    Searching,
    Cancelled,
    Finished,
}

#[derive(Clone, Default)]
struct SearchCancelState {
    cancelled: Arc<AtomicBool>,
}

struct ProgressReporter {
    total: usize,
    completed: usize,
    app: Option<AppHandle>,
    last_emit: Option<Instant>,
    emit_interval: Duration,
}

impl ProgressReporter {
    fn new(app: Option<AppHandle>) -> Self {
        Self {
            total: 0,
            completed: 0,
            app,
            last_emit: None,
            emit_interval: Duration::from_millis(100),
        }
    }

    fn set_total(&mut self, total: usize) {
        self.total = total;
        self.completed = 0;
        self.emit(ProgressStage::Merging, Path::new(""));
    }

    fn tick(&mut self, path: &Path) {
        self.completed = self.completed.saturating_add(1).min(self.total);

        if self.completed == self.total || self.should_emit() {
            self.emit(ProgressStage::Merging, path);
        }
    }

    fn finish(&mut self) {
        self.completed = self.total;
        self.emit(ProgressStage::Finished, Path::new(""));
    }

    fn should_emit(&self) -> bool {
        self.last_emit
            .map(|last_emit| last_emit.elapsed() >= self.emit_interval)
            .unwrap_or(true)
    }

    fn emit(&mut self, stage: ProgressStage, path: &Path) {
        if let Some(app) = &self.app {
            let _ = app.emit(
                PROGRESS_EVENT,
                MergeProgress {
                    total: self.total,
                    completed: self.completed,
                    current_path: format_skipped_path(path),
                    stage,
                },
            );
        }

        self.last_emit = Some(Instant::now());
    }
}

struct SearchProgressReporter {
    total: usize,
    scanned_entries: usize,
    matched_entries: usize,
    skipped_count: usize,
    app: Option<AppHandle>,
    last_emit: Option<Instant>,
    emit_interval: Duration,
}

impl SearchProgressReporter {
    fn new(app: Option<AppHandle>) -> Self {
        Self {
            total: 0,
            scanned_entries: 0,
            matched_entries: 0,
            skipped_count: 0,
            app,
            last_emit: None,
            emit_interval: Duration::from_millis(100),
        }
    }

    fn set_total(&mut self, total: usize) {
        self.total = total;
        self.scanned_entries = 0;
        self.matched_entries = 0;
        self.skipped_count = 0;
        self.emit(SearchProgressStage::Searching, Path::new(""));
    }

    fn tick(&mut self, path: &Path, matched_entries: usize, skipped_count: usize) {
        self.scanned_entries = self.scanned_entries.saturating_add(1).min(self.total);
        self.matched_entries = matched_entries;
        self.skipped_count = skipped_count;

        if self.scanned_entries == self.total || self.should_emit() {
            self.emit(SearchProgressStage::Searching, path);
        }
    }

    fn sync_counts(&mut self, matched_entries: usize, skipped_count: usize) {
        self.matched_entries = matched_entries;
        self.skipped_count = skipped_count;
    }

    fn finish(&mut self) {
        self.scanned_entries = self.total;
        self.emit(SearchProgressStage::Finished, Path::new(""));
    }

    fn should_emit(&self) -> bool {
        self.last_emit
            .map(|last_emit| last_emit.elapsed() >= self.emit_interval)
            .unwrap_or(true)
    }

    fn emit(&mut self, stage: SearchProgressStage, path: &Path) {
        if let Some(app) = &self.app {
            let _ = app.emit(
                SEARCH_PROGRESS_EVENT,
                SearchProgress {
                    total: self.total,
                    scanned_entries: self.scanned_entries,
                    matched_entries: self.matched_entries,
                    skipped_count: self.skipped_count,
                    current_path: format_skipped_path(path),
                    stage,
                },
            );
        }

        self.last_emit = Some(Instant::now());
    }
}

#[tauri::command]
async fn merge_directories(
    app: AppHandle,
    original_path: String,
    merge_path: String,
    operation_mode: OperationMode,
    archive_duplicates: bool,
) -> Result<MergeReport, String> {
    let task = tauri::async_runtime::spawn_blocking(move || {
        merge_directories_inner(
            original_path,
            merge_path,
            operation_mode,
            archive_duplicates,
            Some(app),
        )
    });

    match task.await {
        Ok(result) => result.map_err(serialize_merge_error),
        Err(error) => Err(serialize_merge_error(MergeError::new(
            "合并任务异常结束。",
            error.to_string(),
        ))),
    }
}

#[tauri::command]
async fn search_folder(
    app: AppHandle,
    cancel_state: tauri::State<'_, SearchCancelState>,
    root_path: String,
    query: String,
    include_files: bool,
    include_directories: bool,
) -> Result<SearchReport, String> {
    cancel_state.cancelled.store(false, Ordering::SeqCst);
    let cancel_flag = cancel_state.cancelled.clone();
    let task = tauri::async_runtime::spawn_blocking(move || {
        search_folder_inner(
            root_path,
            query,
            include_files,
            include_directories,
            Some(app),
            cancel_flag,
        )
    });

    match task.await {
        Ok(result) => result.map_err(serialize_merge_error),
        Err(error) => Err(serialize_merge_error(MergeError::new(
            "搜索任务异常结束。",
            error.to_string(),
        ))),
    }
}

#[tauri::command]
fn cancel_search(cancel_state: tauri::State<'_, SearchCancelState>) {
    cancel_state.cancelled.store(true, Ordering::SeqCst);
}

#[tauri::command]
fn reveal_in_folder(path: String) -> Result<(), String> {
    reveal_in_folder_inner(path).map_err(serialize_merge_error)
}

#[tauri::command]
fn delete_to_trash(path: String) -> Result<(), String> {
    delete_to_trash_inner(path).map_err(serialize_merge_error)
}

fn serialize_merge_error(error: MergeError) -> String {
    let fallback = error.message.clone();
    serde_json::to_string(&error).unwrap_or(fallback)
}

fn merge_directories_inner(
    original_path: String,
    merge_path: String,
    operation_mode: OperationMode,
    archive_duplicates: bool,
    app: Option<AppHandle>,
) -> Result<MergeReport, MergeError> {
    merge_directories_validated(
        original_path,
        merge_path,
        operation_mode,
        archive_duplicates,
    )
    .and_then(|(original, incoming, options)| merge_directories_with_paths(original, incoming, options, app))
}

fn merge_directories_validated(
    original_path: String,
    merge_path: String,
    operation_mode: OperationMode,
    archive_duplicates: bool,
) -> Result<(PathBuf, PathBuf, MergeOptions), MergeError> {
    let original = PathBuf::from(original_path);
    let incoming = PathBuf::from(merge_path);

    validate_directory(&original, "原始文件夹")?;
    validate_directory(&incoming, "要合并的文件夹")?;

    let original = original
        .canonicalize()
        .map_err(|error| {
            MergeError::new(
                "无法读取原始文件夹。",
                format!("path={}\nerror={error}", original.display()),
            )
        })?;
    let incoming = incoming
        .canonicalize()
        .map_err(|error| {
            MergeError::new(
                "无法读取要合并的文件夹。",
                format!("path={}\nerror={error}", incoming.display()),
            )
        })?;

    if original == incoming {
        return Err(MergeError::new(
            "原始文件夹和要合并的文件夹不能是同一个目录。",
            format!("original={}\nincoming={}", original.display(), incoming.display()),
        ));
    }

    if original.starts_with(&incoming) || incoming.starts_with(&original) {
        return Err(MergeError::new(
            "不能选择互相包含的文件夹，请选择两个独立目录后再合并。",
            format!("original={}\nincoming={}", original.display(), incoming.display()),
        ));
    }

    let options = MergeOptions {
        operation_mode,
        archive_duplicates,
    };

    Ok((original, incoming, options))
}

fn merge_directories_with_paths(
    original: PathBuf,
    incoming: PathBuf,
    options: MergeOptions,
    app: Option<AppHandle>,
) -> Result<MergeReport, MergeError> {
    let mut report = MergeReport::default();
    let mut progress = ProgressReporter::new(app);
    progress.emit(ProgressStage::Scanning, Path::new(""));
    progress.set_total(
        count_merge_tasks(&incoming).map_err(|error| MergeError::from_io("扫描失败。", error))?,
    );

    merge_folder_contents(
        &incoming,
        &original,
        Path::new(""),
        &options,
        &mut report,
        &mut progress,
    )
    .map_err(|error| MergeError::from_io("合并失败。", error))?;
    progress.finish();

    Ok(report)
}

fn validate_directory(path: &Path, label: &str) -> Result<(), MergeError> {
    let metadata = fs::metadata(path).map_err(|error| {
        MergeError::new(
            format!("{label}无法访问。"),
            format!("path={}\nerror={error}", path.display()),
        )
    })?;

    if metadata.is_dir() {
        Ok(())
    } else {
        Err(MergeError::new(
            format!("{label}不是有效目录。"),
            format!("path={}", path.display()),
        ))
    }
}

fn search_folder_inner(
    root_path: String,
    query: String,
    include_files: bool,
    include_directories: bool,
    app: Option<AppHandle>,
    cancel_flag: Arc<AtomicBool>,
) -> Result<SearchReport, MergeError> {
    let root = PathBuf::from(root_path);
    validate_directory(&root, "搜索文件夹")?;

    if !include_files && !include_directories {
        return Err(MergeError::new(
            "搜索条件无效。",
            "请至少勾选文件或文件夹。",
        ));
    }

    let root = root.canonicalize().map_err(|error| {
        MergeError::new(
            "无法读取搜索文件夹。",
            format!("path={}\nerror={error}", root.display()),
        )
    })?;
    let query = query.trim().to_lowercase();
    let mut report = SearchReport::default();
    let mut progress = SearchProgressReporter::new(app);
    progress.emit(SearchProgressStage::Counting, Path::new(""));
    progress.set_total(
        count_search_tasks(&root, &cancel_flag)
            .map_err(|error| MergeError::from_io("搜索统计失败。", error))?,
    );

    if cancel_flag.load(Ordering::SeqCst) {
        report.cancelled = true;
        progress.sync_counts(report.results.len(), report.skipped_count);
        progress.emit(SearchProgressStage::Cancelled, Path::new(""));
        return Ok(report);
    }

    search_folder_recursive(
        &root,
        &query,
        include_files,
        include_directories,
        &mut report,
        &mut progress,
        &cancel_flag,
        false,
    )?;

    if report.cancelled {
        progress.emit(SearchProgressStage::Cancelled, Path::new(""));
    } else {
        progress.finish();
    }

    Ok(report)
}

fn search_folder_recursive(
    dir: &Path,
    query: &str,
    include_files: bool,
    include_directories: bool,
    report: &mut SearchReport,
    progress: &mut SearchProgressReporter,
    cancel_flag: &AtomicBool,
    collect_size: bool,
) -> Result<u64, MergeError> {
    if cancel_flag.load(Ordering::SeqCst) {
        report.cancelled = true;
        return Ok(0);
    }

    let mut dir_size = 0;
    let entries = match read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => {
            report.skipped_count += 1;
            progress.sync_counts(report.results.len(), report.skipped_count);
            return Ok(0);
        }
    };

    for entry in entries {
        if cancel_flag.load(Ordering::SeqCst) {
            report.cancelled = true;
            break;
        }

        let entry = match read_dir_entry(entry, dir) {
            Ok(entry) => entry,
            Err(_) => {
                report.skipped_count += 1;
                progress.sync_counts(report.results.len(), report.skipped_count);
                continue;
            }
        };
        let file_type = match dir_entry_file_type(&entry) {
            Ok(file_type) => file_type,
            Err(_) => {
                report.skipped_count += 1;
                progress.sync_counts(report.results.len(), report.skipped_count);
                continue;
            }
        };

        report.scanned_entries += 1;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = file_type.is_dir();
        let mut size_bytes = 0;
        progress.tick(&path, report.results.len(), report.skipped_count);
        if file_type.is_file() {
            let should_include_file = include_files && matches_query(&name, query);

            if collect_size || should_include_file {
                size_bytes = entry
                    .metadata()
                    .map(|metadata| metadata.len())
                    .unwrap_or_else(|_| {
                        report.skipped_count += 1;
                        progress.sync_counts(report.results.len(), report.skipped_count);
                        0
                    });
            }

            if collect_size {
                dir_size += size_bytes;
            }

            if should_include_file {
                push_search_result(report, &name, &path, false, size_bytes);
                progress.sync_counts(report.results.len(), report.skipped_count);
            }

            continue;
        }

        let should_include_dir = is_dir && include_directories && matches_query(&name, query);

        if is_dir && !file_type.is_symlink() {
            size_bytes = search_folder_recursive(
                &path,
                query,
                include_files,
                include_directories,
                report,
                progress,
                cancel_flag,
                collect_size || should_include_dir,
            )?;

            if collect_size {
                dir_size += size_bytes;
            }
        }

        if should_include_dir {
            push_search_result(report, &name, &path, true, size_bytes);
            progress.sync_counts(report.results.len(), report.skipped_count);
        }
    }

    Ok(if collect_size { dir_size } else { 0 })
}

fn count_search_tasks(dir: &Path, cancel_flag: &AtomicBool) -> io::Result<usize> {
    if cancel_flag.load(Ordering::SeqCst) {
        return Ok(0);
    }

    let mut count = 0;

    for entry in read_dir(dir)? {
        if cancel_flag.load(Ordering::SeqCst) {
            break;
        }

        let entry = read_dir_entry(entry, dir)?;
        let file_type = dir_entry_file_type(&entry)?;
        count += 1;

        if file_type.is_dir() && !file_type.is_symlink() {
            count += count_search_tasks(&entry.path(), cancel_flag)?;
        }
    }

    Ok(count)
}

fn matches_query(name: &str, query: &str) -> bool {
    query.is_empty() || name.to_lowercase().contains(query)
}

fn push_search_result(
    report: &mut SearchReport,
    name: &str,
    path: &Path,
    is_dir: bool,
    size_bytes: u64,
) {
    if report.results.len() >= MAX_SEARCH_RESULTS {
        report.truncated = true;
        return;
    }

    report.results.push(SearchResult {
        name: name.to_string(),
        path: path.to_string_lossy().to_string(),
        parent_path: path
            .parent()
            .map(|parent| parent.to_string_lossy().to_string())
            .unwrap_or_default(),
        is_dir,
        size_bytes,
    });
}

fn reveal_in_folder_inner(path: String) -> Result<(), MergeError> {
    let path = PathBuf::from(path);

    if !path.exists() {
        return Err(MergeError::new(
            "无法打开所在文件夹。",
            format!("path={}\nerror=路径不存在", path.display()),
        ));
    }

    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg("-R").arg(&path).status();

    #[cfg(target_os = "windows")]
    let result = Command::new("explorer").arg("/select,").arg(&path).status();

    #[cfg(all(unix, not(target_os = "macos")))]
    let result = Command::new("xdg-open")
        .arg(path.parent().unwrap_or_else(|| Path::new("/")))
        .status();

    match result {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(MergeError::new(
            "无法打开所在文件夹。",
            format!("path={}\nstatus={status}", path.display()),
        )),
        Err(error) => Err(MergeError::new(
            "无法打开所在文件夹。",
            format!("path={}\nerror={error}", path.display()),
        )),
    }
}

fn delete_to_trash_inner(path: String) -> Result<(), MergeError> {
    let path = PathBuf::from(path);

    if !path.exists() {
        return Err(MergeError::new(
            "无法删除到回收站。",
            format!("path={}\nerror=路径不存在", path.display()),
        ));
    }

    trash::delete(&path).map_err(|error| {
        MergeError::new(
            "无法删除到回收站。",
            format!("path={}\nerror={error}", path.display()),
        )
    })
}

fn io_with_path(error: io::Error, action: &str, path: &Path) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{action}\npath={}\nerror={error}", path.display()),
    )
}

fn io_with_paths(error: io::Error, action: &str, source: &Path, target: &Path) -> io::Error {
    io::Error::new(
        error.kind(),
        format!(
            "{action}\nsource={}\ntarget={}\nerror={error}",
            source.display(),
            target.display()
        ),
    )
}

fn is_storage_full_error(error: &io::Error, details: &str) -> bool {
    error.raw_os_error() == Some(28) || details.contains("No space left on device")
}

fn create_dir_all(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path).map_err(|error| io_with_path(error, "创建目录失败。", path))
}

fn read_dir(path: &Path) -> io::Result<fs::ReadDir> {
    fs::read_dir(path).map_err(|error| io_with_path(error, "读取目录失败。", path))
}

fn read_dir_entry(entry: io::Result<fs::DirEntry>, parent: &Path) -> io::Result<fs::DirEntry> {
    entry.map_err(|error| io_with_path(error, "读取目录项失败。", parent))
}

fn dir_entry_file_type(entry: &fs::DirEntry) -> io::Result<fs::FileType> {
    let path = entry.path();
    entry
        .file_type()
        .map_err(|error| io_with_path(error, "读取文件类型失败。", &path))
}

fn copy_file(source_path: &Path, target_path: &Path) -> io::Result<u64> {
    fs::copy(source_path, target_path)
        .map_err(|error| io_with_paths(error, "复制文件失败。", source_path, target_path))
}

fn rename_path(source_path: &Path, target_path: &Path) -> io::Result<()> {
    fs::rename(source_path, target_path)
        .map_err(|error| io_with_paths(error, "重命名/移动文件失败。", source_path, target_path))
}

fn remove_file(path: &Path) -> io::Result<()> {
    fs::remove_file(path).map_err(|error| io_with_path(error, "删除源文件失败。", path))
}

fn is_missing_source(error: &io::Error, source_path: &Path) -> bool {
    error.kind() == io::ErrorKind::NotFound && !source_path.exists()
}

fn merge_folder_contents(
    source_dir: &Path,
    target_dir: &Path,
    relative_dir: &Path,
    options: &MergeOptions,
    report: &mut MergeReport,
    progress: &mut ProgressReporter,
) -> io::Result<()> {
    create_dir_all(target_dir)?;

    for entry in read_dir(source_dir)? {
        let entry = read_dir_entry(entry, source_dir)?;
        let file_type = dir_entry_file_type(&entry)?;
        let source_path = entry.path();
        let entry_name = entry.file_name();
        let entry_path = Path::new(&entry_name);
        let target_path = target_dir.join(entry_path);
        let relative_path = relative_dir.join(entry_path);

        if file_type.is_symlink() {
            add_skipped_entry(report, format_skipped_path(&relative_path));
            progress.tick(&relative_path);
            continue;
        }

        if file_type.is_dir() {
            merge_directory_entry(
                &source_path,
                &target_path,
                &relative_path,
                options,
                report,
                progress,
            )?;
        } else if file_type.is_file() {
            merge_file_entry(
                &source_path,
                &target_path,
                &relative_path,
                &entry_name,
                target_dir,
                options,
                report,
                progress,
            )?;
        } else {
            add_skipped_entry(report, format_skipped_path(&relative_path));
            progress.tick(&relative_path);
        }
    }

    Ok(())
}

fn merge_directory_entry(
    source_path: &Path,
    target_path: &Path,
    relative_path: &Path,
    options: &MergeOptions,
    report: &mut MergeReport,
    progress: &mut ProgressReporter,
) -> io::Result<()> {
    match file_type(target_path)? {
        None => transfer_directory(source_path, target_path, relative_path, options, report, progress),
        Some(existing_type) if existing_type.is_dir() => {
            report.merged_dirs += 1;
            progress.tick(relative_path);
            merge_folder_contents(
                source_path,
                target_path,
                relative_path,
                options,
                report,
                progress,
            )?;

            if options.operation_mode == OperationMode::Move {
                remove_empty_dir(source_path)?;
            }

            Ok(())
        }
        Some(_) => {
            add_path_conflict(relative_path, report);
            progress.tick(relative_path);
            Ok(())
        }
    }
}

fn merge_file_entry(
    source_path: &Path,
    target_path: &Path,
    relative_path: &Path,
    entry_name: &OsString,
    target_dir: &Path,
    options: &MergeOptions,
    report: &mut MergeReport,
    progress: &mut ProgressReporter,
) -> io::Result<()> {
    let result = match file_type(target_path)? {
        Some(existing_type) if existing_type.is_file() && options.archive_duplicates => {
            archive_duplicate_file(source_path, relative_path, entry_name, target_dir, options, report)
        }
        Some(existing_type) if existing_type.is_file() => {
            record_duplicate(report, relative_path, file_size(source_path));
            add_skipped_entry(
                report,
                format!("重复文件未处理: {}", format_skipped_path(relative_path)),
            );
            Ok(true)
        }
        Some(_) => {
            add_path_conflict(relative_path, report);
            Ok(true)
        }
        None => transfer_file(source_path, target_path, relative_path, options, report),
    };

    if result.is_ok() {
        progress.tick(relative_path);
    }

    result.map(|_| ())
}

fn archive_duplicate_file(
    source_path: &Path,
    relative_path: &Path,
    entry_name: &OsString,
    target_dir: &Path,
    options: &MergeOptions,
    report: &mut MergeReport,
) -> io::Result<bool> {
    let archive_path = archive_destination(target_dir, entry_name, true)?;
    // 移动模式下来源文件会消失，必须在转移前读取大小。
    let size_bytes = file_size(source_path);
    if transfer_file(source_path, &archive_path, relative_path, options, report)? {
        record_duplicate(report, relative_path, size_bytes);
        return Ok(true);
    }

    Ok(false)
}

fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map(|metadata| metadata.len()).unwrap_or(0)
}

fn record_duplicate(report: &mut MergeReport, relative_path: &Path, size_bytes: u64) {
    report.duplicate_files += 1;
    report.duplicate_size_bytes = report.duplicate_size_bytes.saturating_add(size_bytes);
    report.duplicate_entries.push(DuplicateEntry {
        path_segments: relative_path
            .components()
            .map(|component| component.as_os_str().to_string_lossy().to_string())
            .collect(),
        size_bytes,
    });
}

fn add_path_conflict(relative_path: &Path, report: &mut MergeReport) {
    report.path_conflicts += 1;
    add_skipped_entry(
        report,
        format!("路径类型冲突: {}", format_skipped_path(relative_path)),
    );
}

fn transfer_directory(
    source_dir: &Path,
    target_dir: &Path,
    relative_dir: &Path,
    options: &MergeOptions,
    report: &mut MergeReport,
    progress: &mut ProgressReporter,
) -> io::Result<()> {
    create_dir_all(target_dir)?;
    report.created_dirs += 1;
    progress.tick(relative_dir);

    for entry in read_dir(source_dir)? {
        let entry = read_dir_entry(entry, source_dir)?;
        let file_type = dir_entry_file_type(&entry)?;
        let source_path = entry.path();
        let entry_name = entry.file_name();
        let entry_path = Path::new(&entry_name);
        let target_path = target_dir.join(entry_path);
        let relative_path = relative_dir.join(entry_path);

        if file_type.is_symlink() {
            add_skipped_entry(report, format_skipped_path(&relative_path));
            progress.tick(&relative_path);
            continue;
        }

        if file_type.is_dir() {
            transfer_directory(&source_path, &target_path, &relative_path, options, report, progress)?;
        } else if file_type.is_file() {
            transfer_file(&source_path, &target_path, &relative_path, options, report)?;
            progress.tick(&relative_path);
        } else {
            add_skipped_entry(report, format_skipped_path(&relative_path));
            progress.tick(&relative_path);
        }
    }

    if options.operation_mode == OperationMode::Move {
        remove_empty_dir(source_dir)?;
    }

    Ok(())
}

fn count_merge_tasks(source_dir: &Path) -> io::Result<usize> {
    let mut count = 0;

    for entry in read_dir(source_dir)? {
        let entry = read_dir_entry(entry, source_dir)?;
        let file_type = dir_entry_file_type(&entry)?;
        count += 1;

        if file_type.is_dir() {
            count += count_merge_tasks(&entry.path())?;
        }
    }

    Ok(count)
}

fn add_skipped_entry(report: &mut MergeReport, entry: String) {
    report.skipped_count += 1;
    report.skipped_entries.push(entry);
}

fn transfer_file(
    source_path: &Path,
    target_path: &Path,
    relative_path: &Path,
    options: &MergeOptions,
    report: &mut MergeReport,
) -> io::Result<bool> {
    match options.operation_mode {
        OperationMode::Copy => {
            if let Err(error) = copy_file(source_path, target_path) {
                if is_missing_source(&error, source_path) {
                    add_missing_source_entry(relative_path, report);
                    return Ok(false);
                }

                return Err(error);
            }

            report.copied_files += 1;
        }
        OperationMode::Move => {
            if !move_file(source_path, target_path, relative_path, report)? {
                return Ok(false);
            }

            report.moved_files += 1;
        }
    }

    Ok(true)
}

fn move_file(
    source_path: &Path,
    target_path: &Path,
    relative_path: &Path,
    report: &mut MergeReport,
) -> io::Result<bool> {
    match rename_path(source_path, target_path) {
        Ok(()) => Ok(true),
        Err(rename_error) => {
            if is_missing_source(&rename_error, source_path) {
                add_missing_source_entry(relative_path, report);
                return Ok(false);
            }

            if let Err(copy_error) = copy_file(source_path, target_path) {
                if is_missing_source(&copy_error, source_path) {
                    add_missing_source_entry(relative_path, report);
                    return Ok(false);
                }

                return Err(io::Error::new(
                    copy_error.kind(),
                    format!(
                        "移动文件失败，rename 和 copy fallback 都未成功。\nrename_error={rename_error}\ncopy_error={copy_error}"
                    ),
                ));
            }

            match remove_file(source_path) {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
                Err(error) => Err(error),
            }
        }
    }
}

fn add_missing_source_entry(relative_path: &Path, report: &mut MergeReport) {
    add_skipped_entry(
        report,
        format!("源文件不存在，已跳过: {}", format_skipped_path(relative_path)),
    );
}

fn remove_empty_dir(path: &Path) -> io::Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn archive_destination(target_dir: &Path, entry_name: &OsString, is_file: bool) -> io::Result<PathBuf> {
    let duplicate_dir = duplicate_dir_for(target_dir)?;
    Ok(unique_path(
        &duplicate_dir.join(Path::new(entry_name)),
        is_file,
    ))
}

fn duplicate_dir_for(target_dir: &Path) -> io::Result<PathBuf> {
    let preferred_dir = target_dir.join(DUPLICATE_DIR_NAME);

    match file_type(&preferred_dir)? {
        None => {
            fs::create_dir_all(&preferred_dir)?;
            return Ok(preferred_dir);
        }
        Some(existing_type) if existing_type.is_dir() => return Ok(preferred_dir),
        Some(_) => {}
    }

    for index in 1.. {
        let candidate = target_dir.join(format!("{DUPLICATE_DIR_NAME} ({index})"));

        match file_type(&candidate)? {
            None => {
                fs::create_dir_all(&candidate)?;
                return Ok(candidate);
            }
            Some(existing_type) if existing_type.is_dir() => return Ok(candidate),
            Some(_) => {}
        }
    }

    unreachable!("duplicate_dir_for always returns from the unbounded loop");
}

fn file_type(path: &Path) -> io::Result<Option<fs::FileType>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata.file_type())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn unique_path(path: &Path, is_file: bool) -> PathBuf {
    if path_is_available(path) {
        return path.to_path_buf();
    }

    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "item".to_string());

    for index in 1.. {
        let candidate_name = if is_file {
            unique_file_name(path, index)
        } else {
            OsString::from(format!("{file_name} ({index})"))
        };
        let candidate = parent.join(candidate_name);

        if path_is_available(&candidate) {
            return candidate;
        }
    }

    unreachable!("unique_path always returns from the unbounded loop");
}

fn path_is_available(path: &Path) -> bool {
    matches!(
        fs::symlink_metadata(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    )
}

fn unique_file_name(path: &Path, index: usize) -> OsString {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "file".to_string())
        });

    match path.extension().map(|extension| extension.to_string_lossy()) {
        Some(extension) if !extension.is_empty() => {
            OsString::from(format!("{stem} ({index}).{extension}"))
        }
        _ => OsString::from(format!("{stem} ({index})")),
    }
}

fn format_skipped_path(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(SearchCancelState::default())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            merge_directories,
            search_folder,
            cancel_search,
            reveal_in_folder,
            delete_to_trash
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDirs {
        root: PathBuf,
        original: PathBuf,
        incoming: PathBuf,
    }

    impl TestDirs {
        fn new(name: &str) -> Self {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "dir-merge-{name}-{}-{timestamp}",
                std::process::id()
            ));
            let original = root.join("original");
            let incoming = root.join("incoming");

            fs::create_dir_all(&original).expect("create original test directory");
            fs::create_dir_all(&incoming).expect("create incoming test directory");

            Self {
                root,
                original,
                incoming,
            }
        }
    }

    impl Drop for TestDirs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }

        fs::write(path, contents).expect("write test file");
    }

    fn merge_test_dirs(dirs: &TestDirs) -> MergeReport {
        merge_test_dirs_with_options(dirs, OperationMode::Copy, true)
    }

    fn merge_test_dirs_with_options(
        dirs: &TestDirs,
        operation_mode: OperationMode,
        archive_duplicates: bool,
    ) -> MergeReport {
        merge_directories_inner(
            dirs.original.to_string_lossy().to_string(),
            dirs.incoming.to_string_lossy().to_string(),
            operation_mode,
            archive_duplicates,
            None,
        )
        .expect("merge should succeed")
    }

    #[test]
    fn merging_same_named_directories_does_not_create_duplicate_dir() {
        let dirs = TestDirs::new("no-duplicate-files");

        write_file(&dirs.original.join("shared/original.txt"), "original");
        write_file(&dirs.incoming.join("shared/incoming.txt"), "incoming");

        let report = merge_test_dirs(&dirs);

        assert!(dirs.original.join("shared/original.txt").is_file());
        assert!(dirs.original.join("shared/incoming.txt").is_file());
        assert!(!dirs.original.join(DUPLICATE_DIR_NAME).exists());
        assert!(!dirs.original.join("shared").join(DUPLICATE_DIR_NAME).exists());
        assert_eq!(report.duplicate_files, 0);
        assert_eq!(report.path_conflicts, 0);
    }

    #[test]
    fn duplicate_files_are_archived_in_current_target_directory() {
        let dirs = TestDirs::new("duplicate-files");

        write_file(&dirs.original.join("shared/same.txt"), "original");
        write_file(&dirs.incoming.join("shared/same.txt"), "incoming");

        let report = merge_test_dirs(&dirs);

        assert_eq!(
            fs::read_to_string(dirs.original.join("shared/same.txt"))
                .expect("read original file"),
            "original"
        );
        assert_eq!(
            fs::read_to_string(
                dirs.original
                    .join("shared")
                    .join(DUPLICATE_DIR_NAME)
                    .join("same.txt")
            )
            .expect("read duplicate file"),
            "incoming"
        );
        assert_eq!(report.duplicate_files, 1);
        assert_eq!(report.duplicate_size_bytes, 8);
        assert_eq!(
            report.duplicate_entries,
            vec![DuplicateEntry {
                path_segments: vec!["shared".to_string(), "same.txt".to_string()],
                size_bytes: 8,
            }]
        );
        assert_eq!(report.path_conflicts, 0);
    }

    #[test]
    fn duplicate_entries_keep_folder_depth_and_sum_sizes() {
        let dirs = TestDirs::new("duplicate-tree");

        write_file(&dirs.original.join("top.txt"), "x");
        write_file(&dirs.original.join("a/b/deep.txt"), "x");
        write_file(&dirs.incoming.join("top.txt"), "12");
        write_file(&dirs.incoming.join("a/b/deep.txt"), "12345");
        write_file(&dirs.incoming.join("a/new.txt"), "new");

        let report = merge_test_dirs_with_options(&dirs, OperationMode::Move, true);
        let mut segments: Vec<(Vec<String>, u64)> = report
            .duplicate_entries
            .iter()
            .map(|entry| (entry.path_segments.clone(), entry.size_bytes))
            .collect();
        segments.sort();

        assert_eq!(
            segments,
            vec![
                (
                    vec!["a".to_string(), "b".to_string(), "deep.txt".to_string()],
                    5
                ),
                (vec!["top.txt".to_string()], 2),
            ]
        );
        assert_eq!(report.duplicate_files, 2);
        assert_eq!(report.duplicate_size_bytes, 7);
    }

    #[test]
    fn file_directory_path_conflicts_do_not_create_duplicate_dir() {
        let dirs = TestDirs::new("path-conflict");

        write_file(&dirs.original.join("shared"), "target file");
        write_file(&dirs.incoming.join("shared/incoming.txt"), "incoming");

        let report = merge_test_dirs(&dirs);

        assert!(dirs.original.join("shared").is_file());
        assert!(!dirs.original.join(DUPLICATE_DIR_NAME).exists());
        assert_eq!(report.duplicate_files, 0);
        assert_eq!(report.path_conflicts, 1);
    }

    #[test]
    fn move_mode_leaves_duplicate_file_when_duplicate_archive_is_disabled() {
        let dirs = TestDirs::new("move-skip-duplicate");

        write_file(&dirs.original.join("same.txt"), "original");
        write_file(&dirs.incoming.join("same.txt"), "incoming");
        write_file(&dirs.incoming.join("new.txt"), "new");

        let report = merge_test_dirs_with_options(&dirs, OperationMode::Move, false);

        assert_eq!(
            fs::read_to_string(dirs.original.join("same.txt")).expect("read original duplicate"),
            "original"
        );
        assert_eq!(
            fs::read_to_string(dirs.incoming.join("same.txt")).expect("read incoming duplicate"),
            "incoming"
        );
        assert_eq!(
            fs::read_to_string(dirs.original.join("new.txt")).expect("read moved file"),
            "new"
        );
        assert!(!dirs.incoming.join("new.txt").exists());
        assert!(!dirs.original.join(DUPLICATE_DIR_NAME).exists());
        assert_eq!(report.moved_files, 1);
        assert_eq!(report.duplicate_files, 1);
        assert_eq!(report.duplicate_size_bytes, 8);
        assert_eq!(
            report.duplicate_entries,
            vec![DuplicateEntry {
                path_segments: vec!["same.txt".to_string()],
                size_bytes: 8,
            }]
        );
        assert_eq!(report.skipped_count, 1);
    }

    #[test]
    fn move_mode_skips_missing_source_file_without_failing_merge() {
        let dirs = TestDirs::new("move-missing-source");
        let missing_source = dirs.incoming.join("missing.txt");
        let target = dirs.original.join("missing.txt");
        let mut report = MergeReport::default();
        let options = MergeOptions {
            operation_mode: OperationMode::Move,
            archive_duplicates: false,
        };

        let transferred = transfer_file(
            &missing_source,
            &target,
            Path::new("missing.txt"),
            &options,
            &mut report,
        )
        .expect("missing source should be skipped, not fail");

        assert!(!transferred);
        assert_eq!(report.moved_files, 0);
        assert_eq!(report.skipped_count, 1);
        assert_eq!(
            report.skipped_entries,
            vec!["源文件不存在，已跳过: missing.txt"]
        );
    }

    #[test]
    fn merge_task_count_includes_directories_files_and_skipped_entries() {
        let dirs = TestDirs::new("task-count");

        write_file(&dirs.incoming.join("file.txt"), "file");
        write_file(&dirs.incoming.join("folder/nested.txt"), "nested");
        fs::create_dir_all(dirs.incoming.join("empty")).expect("create empty directory");

        assert_eq!(count_merge_tasks(&dirs.incoming).expect("count tasks"), 4);
    }

    #[test]
    fn cancelled_search_returns_partial_report_without_error() {
        let dirs = TestDirs::new("cancel-search");
        let cancel_flag = Arc::new(AtomicBool::new(true));

        write_file(&dirs.incoming.join("file.txt"), "file");

        let report = search_folder_inner(
            dirs.incoming.to_string_lossy().to_string(),
            String::new(),
            true,
            true,
            None,
            cancel_flag,
        )
        .expect("cancelled search should still return a report");

        assert!(report.cancelled);
        assert!(report.results.is_empty());
    }

    #[test]
    fn folder_search_only_calculates_size_for_matched_directories() {
        let dirs = TestDirs::new("matched-folder-size");

        write_file(&dirs.incoming.join("parent/matched/file.txt"), "12345");
        write_file(&dirs.incoming.join("parent/other/file.txt"), "123456789");

        let report = search_folder_inner(
            dirs.incoming.to_string_lossy().to_string(),
            "matched".to_string(),
            false,
            true,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect("search should succeed");

        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].name, "matched");
        assert_eq!(report.results[0].size_bytes, 5);
    }

    #[test]
    fn invalid_directory_returns_structured_error() {
        let dirs = TestDirs::new("structured-error");
        let missing = dirs.root.join("missing");

        let error = merge_directories_inner(
            dirs.original.to_string_lossy().to_string(),
            missing.to_string_lossy().to_string(),
            OperationMode::Copy,
            true,
            None,
        )
        .expect_err("missing directory should fail");

        assert!(error.message.contains("要合并的文件夹无法访问"));
        assert!(error.details.contains("path="));
        assert!(!error.backtrace.is_empty());
    }

    #[test]
    fn storage_full_error_gets_clear_user_message() {
        let io_error = io_with_path(
            io::Error::from_raw_os_error(28),
            "创建目录失败。",
            Path::new("/Volumes/2T-SSD/target"),
        );
        let error = MergeError::from_io("合并失败。", io_error);

        assert!(error.message.contains("磁盘空间不足"));
        assert!(error.details.contains("创建目录失败"));
        assert!(error.details.contains("/Volumes/2T-SSD/target"));
        assert!(error.details.contains("No space left on device"));
    }
}
