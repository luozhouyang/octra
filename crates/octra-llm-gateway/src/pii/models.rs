//! On-demand download and installation of local PII models.
//!
//! Nothing is downloaded at boot or on the first request. A download starts
//! only when the control plane asks for it, is single-flight per model, and
//! can be cancelled. Partially downloaded files are kept as `.part` so a retry
//! resumes instead of starting over.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::catalog::{ModelFile, ModelSpec};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DownloadState {
    Idle,
    Downloading { downloaded: u64, total: u64 },
    Verifying,
    Ready,
    Failed { message: String },
    Cancelled,
}

impl DownloadState {
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Idle | Self::Ready | Self::Failed { .. } | Self::Cancelled
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelStatus {
    pub id: String,
    pub display_name: String,
    pub license: String,
    pub revision: String,
    pub size: u64,
    pub installed: bool,
    pub state: DownloadState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub default_threshold: f32,
    pub floor: f32,
}

struct DownloadEntry {
    cancel: CancellationToken,
    state: RwLock<DownloadState>,
}

pub struct LocalModelManager {
    root: PathBuf,
    client: reqwest::Client,
    downloads: Mutex<HashMap<&'static str, Arc<DownloadEntry>>>,
}

impl LocalModelManager {
    pub fn new(root: PathBuf) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(3_600))
            .user_agent(concat!("octra/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Self {
            root,
            client,
            downloads: Mutex::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn model_dir(&self, spec: &ModelSpec) -> PathBuf {
        spec.dir(&self.root)
    }

    pub fn status(&self, spec: &ModelSpec) -> ModelStatus {
        let installed = spec.installed(&self.root);
        let state = self
            .downloads
            .lock()
            .map(|downloads| {
                downloads
                    .get(spec.id)
                    .map(|entry| {
                        entry.state.read().map(|state| state.clone()).unwrap_or(
                            DownloadState::Failed {
                                message: "download state is unavailable".into(),
                            },
                        )
                    })
                    .unwrap_or(DownloadState::Idle)
            })
            .unwrap_or(DownloadState::Idle);
        let state = if installed {
            DownloadState::Ready
        } else if matches!(state, DownloadState::Ready) {
            DownloadState::Idle
        } else {
            state
        };
        let last_error = match &state {
            DownloadState::Failed { message } => Some(message.clone()),
            _ => None,
        };
        ModelStatus {
            id: spec.id.to_string(),
            display_name: spec.display_name.to_string(),
            license: spec.license.to_string(),
            revision: spec.revision.to_string(),
            size: spec.total_size(),
            installed,
            state,
            last_error,
            default_threshold: spec.default_threshold,
            floor: spec.floor,
        }
    }

    /// Starts a download if the model is not installed and no download is in
    /// flight. Returns the current status.
    pub fn start(&self, spec: &'static ModelSpec) -> Result<ModelStatus, String> {
        if spec.installed(&self.root) {
            return Ok(self.status(spec));
        }
        {
            let mut downloads = self
                .downloads
                .lock()
                .map_err(|_| "download registry is unavailable".to_string())?;
            if let Some(entry) = downloads.get(spec.id) {
                let state = entry
                    .state
                    .read()
                    .map(|state| state.clone())
                    .unwrap_or(DownloadState::Idle);
                if matches!(
                    state,
                    DownloadState::Downloading { .. } | DownloadState::Verifying
                ) {
                    return Ok(self.status(spec));
                }
            }
            let entry = Arc::new(DownloadEntry {
                cancel: CancellationToken::new(),
                state: RwLock::new(DownloadState::Downloading {
                    downloaded: 0,
                    total: spec.total_size(),
                }),
            });
            downloads.insert(spec.id, entry.clone());
            let root = self.root.clone();
            let client = self.client.clone();
            tokio::spawn(async move {
                run_download(spec, root, client, entry).await;
            });
        }
        Ok(self.status(spec))
    }

    pub fn cancel(&self, spec: &ModelSpec) {
        if let Ok(downloads) = self.downloads.lock()
            && let Some(entry) = downloads.get(spec.id)
        {
            entry.cancel.cancel();
        }
    }

    pub fn remove(&self, spec: &ModelSpec) -> Result<(), String> {
        self.cancel(spec);
        let dir = self.model_dir(spec);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|error| format!("cannot remove {}: {error}", dir.display()))?;
        }
        if let Ok(downloads) = self.downloads.lock()
            && let Some(entry) = downloads.get(spec.id)
            && let Ok(mut state) = entry.state.write()
        {
            *state = DownloadState::Idle;
        }
        Ok(())
    }

    /// Waits until the model's download reaches a terminal state. Used by the
    /// control plane to reload the engine registry once the files are ready.
    pub async fn wait(&self, spec: &'static ModelSpec) -> ModelStatus {
        loop {
            let status = self.status(spec);
            if status.installed || status.state.is_terminal() {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

async fn run_download(
    spec: &'static ModelSpec,
    root: PathBuf,
    client: reqwest::Client,
    entry: Arc<DownloadEntry>,
) {
    let total = spec.total_size();
    set_state(
        &entry,
        DownloadState::Downloading {
            downloaded: 0,
            total,
        },
    );

    let dir = spec.dir(&root);
    if let Err(error) = tokio::fs::create_dir_all(&dir).await {
        fail(&entry, format!("cannot create {}: {error}", dir.display()));
        return;
    }

    let mut completed = 0u64;
    for file in spec.files {
        if entry.cancel.is_cancelled() {
            set_state(&entry, DownloadState::Cancelled);
            return;
        }
        match download_file(spec, &root, &client, &entry, file, completed, total).await {
            Ok(()) => completed += file.size,
            Err(DownloadError::Cancelled) => {
                set_state(&entry, DownloadState::Cancelled);
                return;
            }
            Err(DownloadError::Failed(message)) => {
                fail(&entry, message);
                return;
            }
        }
    }

    set_state(&entry, DownloadState::Verifying);
    if spec.installed(&root) {
        set_state(&entry, DownloadState::Ready);
    } else {
        fail(
            &entry,
            "downloaded files do not match the pinned sizes".into(),
        );
    }
}

enum DownloadError {
    Cancelled,
    Failed(String),
}

async fn download_file(
    spec: &ModelSpec,
    root: &Path,
    client: &reqwest::Client,
    entry: &DownloadEntry,
    file: &ModelFile,
    completed: u64,
    total: u64,
) -> Result<(), DownloadError> {
    let target = spec.file_path(root, file);
    if let Ok(metadata) = std::fs::metadata(&target)
        && metadata.len() == file.size
        && verify_sha(&target, file.sha256).await
    {
        set_state(
            entry,
            DownloadState::Downloading {
                downloaded: completed + file.size,
                total,
            },
        );
        return Ok(());
    }
    let _ = std::fs::remove_file(&target);

    let part = target.with_file_name(format!("{}.part", file.file_name));
    let mut offset = std::fs::metadata(&part)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if offset > file.size {
        let _ = std::fs::remove_file(&part);
        offset = 0;
    }

    let mut request = client.get(spec.url(file));
    if offset > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
    }
    let response = request.send().await.map_err(|error| {
        DownloadError::Failed(format!("cannot download {}: {error}", file.file_name))
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(DownloadError::Failed(format!(
            "cannot download {}: HTTP {status}",
            file.file_name
        )));
    }
    let resuming = status == reqwest::StatusCode::PARTIAL_CONTENT && offset > 0;
    if !resuming {
        offset = 0;
    }

    let mut output = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(resuming)
        .truncate(!resuming)
        .open(&part)
        .await
        .map_err(|error| {
            DownloadError::Failed(format!("cannot open {}: {error}", part.display()))
        })?;

    let mut written = offset;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if entry.cancel.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        let chunk = chunk.map_err(|error| {
            DownloadError::Failed(format!("cannot download {}: {error}", file.file_name))
        })?;
        tokio::io::AsyncWriteExt::write_all(&mut output, &chunk)
            .await
            .map_err(|error| {
                DownloadError::Failed(format!("cannot write {}: {error}", part.display()))
            })?;
        written += chunk.len() as u64;
        if written > file.size {
            return Err(DownloadError::Failed(format!(
                "{} is larger than the pinned size",
                file.file_name
            )));
        }
        set_state(
            entry,
            DownloadState::Downloading {
                downloaded: completed + written,
                total,
            },
        );
    }
    tokio::io::AsyncWriteExt::flush(&mut output)
        .await
        .map_err(|error| {
            DownloadError::Failed(format!("cannot flush {}: {error}", part.display()))
        })?;
    drop(output);

    if written != file.size {
        return Err(DownloadError::Failed(format!(
            "{} is incomplete ({written} of {} bytes)",
            file.file_name, file.size
        )));
    }
    set_state(entry, DownloadState::Verifying);
    if !verify_sha(&part, file.sha256).await {
        let _ = std::fs::remove_file(&part);
        return Err(DownloadError::Failed(format!(
            "{} failed its sha256 check",
            file.file_name
        )));
    }
    tokio::fs::rename(&part, &target).await.map_err(|error| {
        DownloadError::Failed(format!("cannot install {}: {error}", target.display()))
    })?;
    Ok(())
}

async fn verify_sha(path: &Path, expected: &str) -> bool {
    let path = path.to_path_buf();
    let expected = expected.to_string();
    tokio::task::spawn_blocking(move || file_sha256(&path).as_deref() == Some(expected.as_str()))
        .await
        .unwrap_or(false)
}

/// Sync sha256 used both after download and before a model is loaded.
pub(super) fn file_sha256(path: &Path) -> Option<String> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return None;
    };
    let mut hasher = Sha256::new();
    if std::io::copy(&mut file, &mut hasher).is_err() {
        return None;
    }
    Some(hex::encode(hasher.finalize()))
}

fn set_state(entry: &DownloadEntry, state: DownloadState) {
    if let Ok(mut current) = entry.state.write() {
        *current = state;
    }
}

fn fail(entry: &DownloadEntry, message: String) {
    tracing::warn!(%message, "PII model download failed");
    set_state(entry, DownloadState::Failed { message });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pii::catalog::ASTR_LINK_GUARD;

    #[test]
    fn missing_model_is_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let manager = LocalModelManager::new(dir.path().to_path_buf());
        let status = manager.status(&ASTR_LINK_GUARD);
        assert!(!status.installed);
        assert_eq!(status.size, ASTR_LINK_GUARD.total_size());
        assert!(matches!(status.state, DownloadState::Idle));
    }

    #[tokio::test]
    async fn cancel_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let manager = LocalModelManager::new(dir.path().to_path_buf());
        manager.cancel(&ASTR_LINK_GUARD);
        manager.cancel(&ASTR_LINK_GUARD);
    }

    /// Manual smoke test: downloads the pinned model from Hugging Face and
    /// verifies every sha256. Network required.
    #[tokio::test]
    #[ignore]
    async fn downloads_and_verifies_the_pinned_model() {
        let dir = tempfile::tempdir().unwrap();
        let manager = LocalModelManager::new(dir.path().to_path_buf());
        manager.start(&ASTR_LINK_GUARD).unwrap();
        let status = manager.wait(&ASTR_LINK_GUARD).await;
        assert!(status.installed, "{status:?}");
        assert!(matches!(status.state, DownloadState::Ready));
    }
}
