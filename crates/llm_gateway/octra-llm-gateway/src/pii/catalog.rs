//! Built-in local PII models.
//!
//! The catalogue is pinned in code: download URLs, sizes and sha256 are part of
//! the build, so a model file on disk is only "installed" when it matches the
//! exact bytes this build knows about.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub struct ModelFile {
    /// Path inside the upstream repository.
    pub repo_path: &'static str,
    /// Name the file is saved under, so the ONNX graph finds its external data.
    pub file_name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    pub id: &'static str,
    pub display_name: &'static str,
    pub license: &'static str,
    pub revision: &'static str,
    pub base_url: &'static str,
    pub files: &'static [ModelFile],
    pub default_threshold: f32,
    pub floor: f32,
}

/// AstrLink Guard: 12-layer int8 BERT token classifier for Chinese and English.
pub const ASTR_LINK_GUARD: ModelSpec = ModelSpec {
    id: "astrlink-guard",
    display_name: "AstrLink Guard",
    license: "Apache-2.0",
    revision: "7216b6a3b897ef903d4b1ba367ae6fb95b0fa5e4",
    base_url: "https://huggingface.co/QuantumNous/astrlink-guard/resolve/",
    files: &[
        ModelFile {
            repo_path: "int8/model_int8.onnx",
            file_name: "model_int8.onnx",
            size: 330_781,
            sha256: "0a070d3113615e33ba3b84d6bd486f663a377c04c4b056e7381732179f51832a",
        },
        ModelFile {
            repo_path: "int8/model_int8.onnx_data",
            file_name: "model_int8.onnx_data",
            size: 37_124_608,
            sha256: "0ba34216ceead01e0ae2dfd8e46d52b67e7aee80b04f722d9b758613a91c0067",
        },
        ModelFile {
            repo_path: "int8/tokenizer.json",
            file_name: "tokenizer.json",
            size: 1_502_881,
            sha256: "0cf3b235ef125658015a6c8026a5fe6f28088bf825101a990ec18a10200feef4",
        },
    ],
    default_threshold: 0.8,
    floor: 0.5,
};

pub const MODELS: &[ModelSpec] = &[ASTR_LINK_GUARD];

pub fn get(id: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|spec| spec.id == id)
}

impl ModelSpec {
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|file| file.size).sum()
    }

    /// Revision-scoped directory so an upgrade never reads stale weights.
    pub fn dir(&self, root: &Path) -> PathBuf {
        let revision = &self.revision[..self.revision.len().min(12)];
        root.join(self.id).join(revision)
    }

    pub fn file_path(&self, root: &Path, file: &ModelFile) -> PathBuf {
        self.dir(root).join(file.file_name)
    }

    pub fn url(&self, file: &ModelFile) -> String {
        format!("{}{}/{}", self.base_url, self.revision, file.repo_path)
    }

    /// Installed means every file is present at the pinned size. Full sha256
    /// verification happens on download and again before loading.
    pub fn installed(&self, root: &Path) -> bool {
        self.files.iter().all(|file| {
            std::fs::metadata(self.file_path(root, file))
                .map(|metadata| metadata.len() == file.size)
                .unwrap_or(false)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_entries_are_pinned() {
        for spec in MODELS {
            assert!(!spec.files.is_empty());
            assert!(spec.default_threshold >= spec.floor && spec.default_threshold < 1.0);
            for file in spec.files {
                assert_eq!(file.sha256.len(), 64);
                assert!(file.size > 0);
            }
        }
    }

    #[test]
    fn installed_requires_exact_sizes() {
        const FILES: &[ModelFile] = &[
            ModelFile {
                repo_path: "one.bin",
                file_name: "one.bin",
                size: 1,
                sha256: "unused",
            },
            ModelFile {
                repo_path: "two.bin",
                file_name: "two.bin",
                size: 2,
                sha256: "unused",
            },
        ];
        const SPEC: ModelSpec = ModelSpec {
            id: "test",
            display_name: "Test",
            license: "test",
            revision: "0123456789abcdef",
            base_url: "https://example.invalid/",
            files: FILES,
            default_threshold: 0.8,
            floor: 0.5,
        };

        let dir = tempfile::tempdir().unwrap();
        assert!(!SPEC.installed(dir.path()));

        let model_dir = SPEC.dir(dir.path());
        std::fs::create_dir_all(&model_dir).unwrap();
        for file in SPEC.files {
            std::fs::write(
                model_dir.join(file.file_name),
                vec![0u8; file.size as usize],
            )
            .unwrap();
        }
        assert!(SPEC.installed(dir.path()));
    }
}
