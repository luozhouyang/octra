//! In-process ONNX Runtime client for AstrLink Guard.
//!
//! The model is a 12-layer int8 BERT token classifier. Text is tokenized once,
//! then fed to the model in 512-position windows; BIO token spans are merged
//! into byte-offset detections before they leave the engine.

use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

use async_trait::async_trait;
use octra_pii_types::{
    DetectRequest, Detection, EngineReadiness, PiiDetectorClient, PiiEngineError,
    PiiEngineErrorKind, PiiKind,
};
use ort::session::Session;
use ort::value::Tensor;
use tokenizers::Tokenizer;

use super::catalog::ModelSpec;
use super::models::file_sha256;

const MODEL_FILE: &str = "model_int8.onnx";
const TOKENIZER_FILE: &str = "tokenizer.json";
const MAX_POSITIONS: usize = 512;
const WINDOW: usize = MAX_POSITIONS - 2;
const WINDOW_OVERLAP: usize = 64;
const DETECTOR: &str = "astrlink-guard";
pub(crate) const DEFAULT_SESSIONS: usize = 2;
pub(crate) const MAX_SESSIONS: usize = 8;

const LABELS: [&str; 21] = [
    "O",
    "B-email",
    "I-email",
    "B-phone",
    "I-phone",
    "B-account",
    "I-account",
    "B-payment_card",
    "I-payment_card",
    "B-ip_address",
    "I-ip_address",
    "B-url",
    "I-url",
    "B-common_secret",
    "I-common_secret",
    "B-private_address",
    "I-private_address",
    "B-private_date",
    "I-private_date",
    "B-private_person",
    "I-private_person",
];

struct LoadedModel {
    tokenizer: Tokenizer,
    pool: SessionPool,
}

/// Fixed-size pool of ONNX sessions. Each session is checked out for one
/// window and returned when the guard drops.
struct SessionPool {
    sessions: Mutex<Vec<Session>>,
    ready: Condvar,
}

impl SessionPool {
    fn new(sessions: Vec<Session>) -> Self {
        Self {
            sessions: Mutex::new(sessions),
            ready: Condvar::new(),
        }
    }

    fn acquire(&self) -> Result<SessionGuard<'_>, PiiEngineError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| engine_error(PiiEngineErrorKind::Unavailable, "ONNX pool is poisoned"))?;
        while sessions.is_empty() {
            sessions = self.ready.wait(sessions).map_err(|_| {
                engine_error(PiiEngineErrorKind::Unavailable, "ONNX pool is poisoned")
            })?;
        }
        Ok(SessionGuard {
            pool: self,
            session: sessions.pop(),
        })
    }
}

struct SessionGuard<'a> {
    pool: &'a SessionPool,
    session: Option<Session>,
}

impl Deref for SessionGuard<'_> {
    type Target = Session;

    fn deref(&self) -> &Session {
        self.session
            .as_ref()
            .expect("session is present until drop")
    }
}

impl DerefMut for SessionGuard<'_> {
    fn deref_mut(&mut self) -> &mut Session {
        self.session
            .as_mut()
            .expect("session is present until drop")
    }
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            let mut sessions = self
                .pool
                .sessions
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            sessions.push(session);
            self.pool.ready.notify_one();
        }
    }
}

pub struct LocalPiiClient {
    spec: &'static ModelSpec,
    threshold: f32,
    inner: Result<Arc<LoadedModel>, String>,
}

impl LocalPiiClient {
    pub fn load(
        spec: &'static ModelSpec,
        dir: &Path,
        threshold: f32,
        sessions: usize,
    ) -> Arc<Self> {
        let inner = load_model(spec, dir, sessions).map(Arc::new);
        if let Err(error) = &inner {
            tracing::warn!(model = spec.id, %error, "local PII model is unavailable");
        }
        Arc::new(Self {
            spec,
            threshold,
            inner,
        })
    }

    pub fn spec(&self) -> &'static ModelSpec {
        self.spec
    }

    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    pub fn error(&self) -> Option<&str> {
        self.inner.as_ref().err().map(String::as_str)
    }
}

fn load_model(
    spec: &'static ModelSpec,
    dir: &Path,
    sessions: usize,
) -> Result<LoadedModel, String> {
    for file in spec.files {
        let path = dir.join(file.file_name);
        let metadata = std::fs::metadata(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if metadata.len() != file.size {
            return Err(format!(
                "{} has {} bytes, expected {}",
                path.display(),
                metadata.len(),
                file.size
            ));
        }
        // The pinned hash is checked again before loading so a same-size
        // tampered file is never fed to ONNX Runtime.
        if file_sha256(&path).as_deref() != Some(file.sha256) {
            return Err(format!("{} failed its sha256 check", path.display()));
        }
    }

    let tokenizer_path = dir.join(TOKENIZER_FILE);
    let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
        .map_err(|error| format!("cannot load {}: {error}", tokenizer_path.display()))?;
    // The engine performs its own windowing; the tokenizer must not truncate.
    tokenizer
        .with_truncation(None)
        .map_err(|error| format!("cannot disable tokenizer truncation: {error}"))?;
    tokenizer.with_padding(None);

    let model_path = dir.join(MODEL_FILE);
    let build_session = || -> Result<Session, String> {
        let session = Session::builder()
            .map_err(|error| format!("cannot create ONNX Runtime session: {error}"))?
            .commit_from_file(&model_path)
            .map_err(|error| format!("cannot load {}: {error}", model_path.display()))?;
        let names: Vec<&str> = session
            .inputs
            .iter()
            .map(|input| input.name.as_str())
            .collect();
        for required in ["input_ids", "attention_mask", "token_type_ids"] {
            if !names.contains(&required) {
                return Err(format!(
                    "model is missing the `{required}` input (inputs: {names:?})"
                ));
            }
        }
        Ok(session)
    };

    let sessions = sessions.clamp(1, MAX_SESSIONS);
    let mut pool = Vec::with_capacity(sessions);
    for _ in 0..sessions {
        pool.push(build_session()?);
    }

    Ok(LoadedModel {
        tokenizer,
        pool: SessionPool::new(pool),
    })
}

impl LoadedModel {
    fn detect(&self, text: &str, threshold: f32) -> Result<Vec<Detection>, PiiEngineError> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|error| engine_error(PiiEngineErrorKind::Rejected, error.to_string()))?;
        let ids = encoding.get_ids();
        let type_ids = encoding.get_type_ids();
        let offsets = encoding.get_offsets();
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let has_cls = offsets.first().is_some_and(|offset| *offset == (0, 0));
        let has_sep = offsets.last().is_some_and(|offset| *offset == (0, 0));
        let middle_start = usize::from(has_cls);
        let middle_end = ids.len().saturating_sub(usize::from(has_sep));
        if middle_end <= middle_start {
            return Ok(Vec::new());
        }

        if ids.len() <= MAX_POSITIONS {
            let window_types = if type_ids.len() == ids.len() {
                type_ids.to_vec()
            } else {
                vec![0; ids.len()]
            };
            let window = run_window(self, ids, &window_types, &offsets[middle_start..middle_end])?;
            return Ok(decode_spans(window, threshold));
        }

        let cls = ids[0];
        let sep = ids[ids.len() - 1];
        let mut detections = Vec::new();
        for (start, end) in window_ranges(middle_start, middle_end) {
            let mut window_ids = Vec::with_capacity(end - start + 2);
            window_ids.push(cls);
            window_ids.extend_from_slice(&ids[start..end]);
            window_ids.push(sep);
            let window_types = vec![0u32; window_ids.len()];
            let window = run_window(self, &window_ids, &window_types, &offsets[start..end])?;
            detections.extend(decode_spans(window, threshold));
        }
        Ok(sorted_merged(detections))
    }
}

/// Windows over content tokens. Consecutive windows share `WINDOW_OVERLAP`
/// tokens so an entity split across a boundary is still seen whole.
fn window_ranges(middle_start: usize, middle_end: usize) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = middle_start;
    loop {
        let end = (start + WINDOW).min(middle_end);
        ranges.push((start, end));
        if end >= middle_end {
            break;
        }
        start = end - WINDOW_OVERLAP;
    }
    ranges
}

fn run_window(
    model: &LoadedModel,
    ids: &[u32],
    type_ids: &[u32],
    content_offsets: &[(usize, usize)],
) -> Result<Vec<TokenPred>, PiiEngineError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    if ids.len() < 2 || content_offsets.len() + 2 != ids.len() {
        return Err(engine_error(
            PiiEngineErrorKind::Protocol,
            "the tokenizer did not add the expected special tokens",
        ));
    }
    let ids: Vec<i64> = ids.iter().map(|id| i64::from(*id)).collect();
    let type_ids: Vec<i64> = type_ids.iter().map(|id| i64::from(*id)).collect();
    let attention: Vec<i64> = vec![1; ids.len()];

    let input_ids =
        Tensor::from_array(([1usize, ids.len()], ids.into_boxed_slice())).map_err(ort_error)?;
    let attention_mask =
        Tensor::from_array(([1usize, attention.len()], attention.into_boxed_slice()))
            .map_err(ort_error)?;
    let token_type_ids =
        Tensor::from_array(([1usize, type_ids.len()], type_ids.into_boxed_slice()))
            .map_err(ort_error)?;

    let mut session = model.pool.acquire()?;
    let outputs = session
        .run(ort::inputs![
            "input_ids" => input_ids,
            "attention_mask" => attention_mask,
            "token_type_ids" => token_type_ids,
        ])
        .map_err(ort_error)?;
    let (shape, data) = outputs[0].try_extract_tensor::<f32>().map_err(ort_error)?;
    if shape.len() != 3 || shape[0] != 1 {
        return Err(engine_error(
            PiiEngineErrorKind::Protocol,
            format!("unexpected model output shape {shape:?}"),
        ));
    }
    let sequence = shape[1] as usize;
    let labels = shape[2] as usize;
    if labels != LABELS.len() || data.len() != sequence * labels {
        return Err(engine_error(
            PiiEngineErrorKind::Protocol,
            "model output does not match the pinned label set",
        ));
    }

    let mut predictions = Vec::with_capacity(sequence);
    for position in 1..sequence.saturating_sub(1) {
        let logits = &data[position * labels..(position + 1) * labels];
        let (label, confidence) = softmax_max(logits);
        let Some(offset) = content_offsets.get(position - 1).copied() else {
            continue;
        };
        predictions.push(TokenPred {
            label: LABELS[label],
            offset,
            confidence,
        });
    }
    Ok(predictions)
}

#[derive(Debug, Clone, Copy)]
struct TokenPred {
    label: &'static str,
    offset: (usize, usize),
    confidence: f32,
}

fn decode_spans(predictions: Vec<TokenPred>, threshold: f32) -> Vec<Detection> {
    let mut detections = Vec::new();
    let mut current: Option<(PiiKind, String, usize, usize, f32)> = None;

    for prediction in predictions {
        if prediction.offset.0 == prediction.offset.1 {
            continue;
        }
        let (bio, label) = prediction
            .label
            .split_once('-')
            .unwrap_or(("O", prediction.label));
        match bio {
            "B" => {
                close_span(&mut current, &mut detections, threshold);
                if let Some(kind) = kind_for(label) {
                    current = Some((
                        kind,
                        label.to_string(),
                        prediction.offset.0,
                        prediction.offset.1,
                        prediction.confidence,
                    ));
                }
            }
            "I" => {
                let continues = current
                    .as_ref()
                    .is_some_and(|(_, current_label, _, _, _)| current_label == label);
                if continues {
                    if let Some((_, _, _, end, confidence)) = current.as_mut() {
                        *end = prediction.offset.1;
                        *confidence = confidence.min(prediction.confidence);
                    }
                } else {
                    close_span(&mut current, &mut detections, threshold);
                }
            }
            _ => close_span(&mut current, &mut detections, threshold),
        }
    }
    close_span(&mut current, &mut detections, threshold);
    sorted_merged(detections)
}

fn sorted_merged(mut detections: Vec<Detection>) -> Vec<Detection> {
    detections.sort_by(|left, right| {
        left.start
            .cmp(&right.start)
            .then_with(|| left.end.cmp(&right.end))
    });
    merge_overlaps(detections)
}

fn close_span(
    current: &mut Option<(PiiKind, String, usize, usize, f32)>,
    detections: &mut Vec<Detection>,
    threshold: f32,
) {
    if let Some((kind, _, start, end, confidence)) = current.take()
        && start < end
        && confidence >= threshold
    {
        detections.push(Detection {
            kind,
            start,
            end,
            confidence,
            detector: DETECTOR,
        });
    }
}

fn merge_overlaps(detections: Vec<Detection>) -> Vec<Detection> {
    let mut merged: Vec<Detection> = Vec::with_capacity(detections.len());
    for detection in detections {
        match merged.last_mut() {
            Some(last) if last.kind == detection.kind && detection.start <= last.end => {
                last.end = last.end.max(detection.end);
                last.confidence = last.confidence.max(detection.confidence);
            }
            _ => merged.push(detection),
        }
    }
    merged
}

fn kind_for(label: &str) -> Option<PiiKind> {
    match label {
        "email" => Some(PiiKind::Email),
        "phone" => Some(PiiKind::Phone),
        "payment_card" => Some(PiiKind::BankCard),
        "ip_address" => Some(PiiKind::Ip),
        "account" | "common_secret" | "url" | "private_address" | "private_date"
        | "private_person" => Some(PiiKind::Custom(label.to_string())),
        _ => None,
    }
}

fn softmax_max(logits: &[f32]) -> (usize, f32) {
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let mut sum = 0.0f32;
    for logit in logits {
        sum += (*logit - max).exp();
    }
    let (index, value) = logits
        .iter()
        .enumerate()
        .map(|(index, logit)| (index, ((*logit - max).exp() / sum).max(0.0)))
        .max_by(|left, right| {
            left.1
                .partial_cmp(&right.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or((0, 0.0));
    (index, value)
}

fn ort_error(error: ort::Error) -> PiiEngineError {
    engine_error(PiiEngineErrorKind::Unavailable, error.to_string())
}

fn engine_error(kind: PiiEngineErrorKind, message: impl Into<String>) -> PiiEngineError {
    PiiEngineError::new(kind, message)
}

fn detect_text(
    model: &LoadedModel,
    text: &str,
    threshold: f32,
    kinds: &[PiiKind],
) -> Result<Vec<Detection>, PiiEngineError> {
    let detections = model.detect(text, threshold)?;
    Ok(if kinds.is_empty() {
        detections
    } else {
        detections
            .into_iter()
            .filter(|detection| kinds.contains(&detection.kind))
            .collect()
    })
}

#[async_trait]
impl PiiDetectorClient for LocalPiiClient {
    fn readiness(&self) -> EngineReadiness {
        if self.inner.is_ok() {
            EngineReadiness::Ready
        } else {
            EngineReadiness::Failed
        }
    }

    async fn detect(&self, request: DetectRequest<'_>) -> Result<Vec<Detection>, PiiEngineError> {
        let model = match &self.inner {
            Ok(model) => model.clone(),
            Err(error) => {
                return Err(engine_error(PiiEngineErrorKind::Unavailable, error.clone()));
            }
        };
        if request.text.len() > request.max_bytes {
            return Err(engine_error(
                PiiEngineErrorKind::Rejected,
                format!(
                    "PII input is {} bytes, over the limit of {}",
                    request.text.len(),
                    request.max_bytes
                ),
            ));
        }
        let text = request.text.to_string();
        let threshold = self.threshold;
        let kinds: Vec<PiiKind> = request.kinds.to_vec();
        tokio::task::spawn_blocking(move || detect_text(&model, &text, threshold, &kinds))
            .await
            .map_err(|error| {
                engine_error(
                    PiiEngineErrorKind::Unavailable,
                    format!("PII inference task failed: {error}"),
                )
            })?
    }

    async fn detect_batch(
        &self,
        requests: &[DetectRequest<'_>],
    ) -> Result<Vec<Vec<Detection>>, PiiEngineError> {
        let model = match &self.inner {
            Ok(model) => model.clone(),
            Err(error) => {
                return Err(engine_error(PiiEngineErrorKind::Unavailable, error.clone()));
            }
        };
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let mut jobs = Vec::with_capacity(requests.len());
        for request in requests {
            if request.text.len() > request.max_bytes {
                return Err(engine_error(
                    PiiEngineErrorKind::Rejected,
                    format!(
                        "PII input is {} bytes, over the limit of {}",
                        request.text.len(),
                        request.max_bytes
                    ),
                ));
            }
            jobs.push((request.text.to_string(), request.kinds.to_vec()));
        }
        let threshold = self.threshold;
        let mut handles = Vec::with_capacity(jobs.len());
        for (text, kinds) in jobs {
            let model = model.clone();
            handles.push(tokio::task::spawn_blocking(move || {
                detect_text(&model, &text, threshold, &kinds)
            }));
        }
        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            results.push(handle.await.map_err(|error| {
                engine_error(
                    PiiEngineErrorKind::Unavailable,
                    format!("PII inference task failed: {error}"),
                )
            })??);
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pred(label: &'static str, start: usize, end: usize, confidence: f32) -> TokenPred {
        TokenPred {
            label,
            offset: (start, end),
            confidence,
        }
    }

    #[test]
    fn bio_spans_merge_and_apply_the_threshold() {
        let spans = decode_spans(
            vec![
                pred("B-private_person", 0, 3, 0.95),
                pred("I-private_person", 3, 7, 0.9),
                pred("O", 7, 8, 0.99),
                pred("B-email", 9, 20, 0.6),
            ],
            0.8,
        );
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, PiiKind::Custom("private_person".into()));
        assert_eq!((spans[0].start, spans[0].end), (0, 7));
        assert!((spans[0].confidence - 0.9).abs() < 1e-6);
    }

    #[test]
    fn softmax_picks_the_largest_logit() {
        let (index, probability) = softmax_max(&[0.0, 3.0, 1.0]);
        assert_eq!(index, 1);
        assert!(probability > 0.8);
    }

    #[test]
    fn windows_overlap_and_cover_the_input() {
        let ranges = window_ranges(0, 2_000);
        assert!(ranges.len() > 1);
        assert_eq!(ranges[0], (0, WINDOW));
        for pair in ranges.windows(2) {
            assert_eq!(pair[1].0, pair[0].1 - WINDOW_OVERLAP);
        }
        assert_eq!(ranges.last().unwrap().1, 2_000);
        assert_eq!(window_ranges(0, 100), vec![(0, 100)]);
    }

    #[test]
    fn loading_rejects_a_tampered_file_before_starting_onnx() {
        const FILES: &[super::super::catalog::ModelFile] = &[super::super::catalog::ModelFile {
            repo_path: "weights.bin",
            file_name: "weights.bin",
            size: 1,
            sha256: "4bf5122f344554c53bde2ebb8cd2b7e3d1600ad631c385a5d7cce23c7785459a",
        }];
        const SPEC: super::super::catalog::ModelSpec = super::super::catalog::ModelSpec {
            id: "tampered",
            display_name: "Tampered",
            license: "test",
            revision: "0123456789abcdef",
            base_url: "https://example.invalid/",
            files: FILES,
            default_threshold: 0.8,
            floor: 0.5,
        };

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("weights.bin"), [0x02u8]).unwrap();
        let error = load_model(&SPEC, dir.path(), 1).err().unwrap();
        assert!(error.contains("sha256"), "{error}");

        std::fs::write(dir.path().join("weights.bin"), [0x01u8]).unwrap();
        let error = load_model(&SPEC, dir.path(), 1).err().unwrap();
        assert!(!error.contains("sha256"), "{error}");
    }

    /// Manual smoke test: set `OCTRA_PII_MODEL_DIR` to a directory holding the
    /// three pinned AstrLink Guard files.
    #[tokio::test]
    #[ignore]
    async fn real_model_detects_an_email() {
        let dir = std::env::var("OCTRA_PII_MODEL_DIR").expect("OCTRA_PII_MODEL_DIR");
        let client = LocalPiiClient::load(
            &super::super::catalog::ASTR_LINK_GUARD,
            Path::new(&dir),
            0.5,
            1,
        );
        let detections = client
            .detect(DetectRequest {
                text: "Contact alice@example.com for details.",
                locale: Some("en"),
                kinds: &[],
                max_bytes: 4096,
            })
            .await
            .unwrap();
        assert!(
            detections
                .iter()
                .any(|detection| detection.kind == PiiKind::Email),
            "detections: {detections:?}"
        );
    }
}
