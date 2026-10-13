//! The upload format of a material: one request body, manifest first.
//!
//! ```text
//! "NMB1"                      4 bytes, format magic and version
//! manifest length             4 bytes, big-endian
//! manifest                    JSON UploadManifest, at most max_manifest_bytes
//! blob, blob, …               the bytes of every distinct sha256, in order of
//!                             first appearance in the manifest, each exactly
//!                             as long as its declared size; nothing after
//! ```
//!
//! One request rather than a session of chunk uploads: an upload is either
//! received completely and committed, or not at all, so no half-uploaded
//! material can ever outlive the request that carried it. Manifest first,
//! so every bound is checked against the *declared* sizes before a single
//! blob byte is read — an oversize upload is refused without storing
//! anything — and the actual bytes are then held to exactly what was
//! declared: a short body, an extra byte or a content digest that differs
//! from the manifest refuses the whole upload.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::Path;

use bytes::Bytes;
use futures::{Stream, StreamExt as _};
use sha2::Digest as _;

use super::{
    blob_order, hex, validate_manifest, MaterialError, MaterialLimits, MaterialResult,
    UploadManifest,
};

const MAGIC: &[u8; 4] = b"NMB1";

/// A material ready to upload: its manifest and the bytes of each blob.
#[derive(Debug, Clone)]
pub struct Bundle {
    /// The manifest.
    pub manifest: UploadManifest,
    blobs: BTreeMap<String, Bytes>,
}

impl Bundle {
    /// A bundle of `manifest` whose blobs are `blobs`, by digest.
    pub fn new(manifest: UploadManifest, blobs: BTreeMap<String, Bytes>) -> Self {
        Self { manifest, blobs }
    }

    /// Total size of the files.
    pub fn total_bytes(&self) -> u64 {
        self.manifest.files.iter().map(|file| file.size).sum()
    }

    /// The request body.
    pub fn encode(&self) -> MaterialResult<Vec<u8>> {
        let manifest = serde_json::to_vec(&self.manifest)
            .map_err(|error| MaterialError::InvalidManifest(error.to_string()))?;
        let length = u32::try_from(manifest.len())
            .map_err(|_| MaterialError::TooLarge("the manifest is too large".to_string()))?;
        let order = blob_order(&self.manifest.files);
        let mut body = Vec::with_capacity(
            8 + manifest.len() + order.iter().map(|(_, size)| *size as usize).sum::<usize>(),
        );
        body.extend_from_slice(MAGIC);
        body.extend_from_slice(&length.to_be_bytes());
        body.extend_from_slice(&manifest);
        for (sha256, size) in order {
            let blob = self.blobs.get(&sha256).ok_or_else(|| {
                MaterialError::InvalidManifest(format!("no content for blob {sha256}"))
            })?;
            if blob.len() as u64 != size {
                return Err(MaterialError::InvalidManifest(format!(
                    "blob {sha256} has {} bytes, the manifest declares {size}",
                    blob.len()
                )));
            }
            body.extend_from_slice(blob);
        }
        Ok(body)
    }
}

/// Reads an uploaded bundle from `body`, validates its manifest against
/// `limits` (with at most `max_total` bytes of files), and writes every blob,
/// verified, into `blobs_dir` as `{sha256}`.
///
/// On any refusal some blobs may already sit in `blobs_dir`; the caller
/// stages it and discards it as a whole ([`super::Staging`]).
pub async fn receive<S, E>(
    body: S,
    limits: &MaterialLimits,
    max_total: u64,
    blobs_dir: &Path,
) -> MaterialResult<UploadManifest>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let mut reader = BodyReader {
        stream: body,
        pending: Bytes::new(),
    };
    let header = reader.read_exact(8).await?;
    if &header[..4] != MAGIC {
        return Err(MaterialError::InvalidManifest(
            "not a material bundle (wrong magic)".to_string(),
        ));
    }
    let length = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if length > limits.max_manifest_bytes {
        return Err(MaterialError::TooLarge(format!(
            "the manifest has {length} bytes, at most {} are allowed",
            limits.max_manifest_bytes
        )));
    }
    let manifest: UploadManifest = serde_json::from_slice(&reader.read_exact(length).await?)
        .map_err(|error| MaterialError::InvalidManifest(error.to_string()))?;
    validate_manifest(&manifest, limits, max_total)?;

    fs::create_dir_all(blobs_dir).map_err(|source| MaterialError::io(blobs_dir, source))?;
    for (sha256, size) in blob_order(&manifest.files) {
        let target = blobs_dir.join(&sha256);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|source| MaterialError::io(&target, source))?;
        let mut hasher = sha2::Sha256::new();
        let mut remaining = size;
        while remaining > 0 {
            let chunk = reader.next_chunk(remaining).await?;
            hasher.update(&chunk);
            file.write_all(&chunk)
                .map_err(|source| MaterialError::io(&target, source))?;
            remaining -= chunk.len() as u64;
        }
        file.sync_all()
            .map_err(|source| MaterialError::io(&target, source))?;
        let actual = hex(&hasher.finalize());
        if actual != sha256 {
            return Err(MaterialError::DigestMismatch(format!(
                "blob declared as {sha256} hashes to {actual}"
            )));
        }
    }
    reader.expect_end().await?;
    Ok(manifest)
}

/// A request body read in exact amounts.
struct BodyReader<S> {
    stream: S,
    pending: Bytes,
}

impl<S, E> BodyReader<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    /// The next bytes of the body, at most `max` of them; an error when the
    /// body ends first.
    async fn next_chunk(&mut self, max: u64) -> MaterialResult<Bytes> {
        while self.pending.is_empty() {
            match self.stream.next().await {
                Some(Ok(chunk)) => self.pending = chunk,
                Some(Err(error)) => {
                    return Err(MaterialError::Incomplete(format!(
                        "the upload could not be read: {error}"
                    )))
                }
                None => {
                    return Err(MaterialError::Incomplete(
                        "the upload ended before every declared byte arrived".to_string(),
                    ))
                }
            }
        }
        let take = self
            .pending
            .len()
            .min(usize::try_from(max).unwrap_or(usize::MAX));
        Ok(self.pending.split_to(take))
    }

    async fn read_exact(&mut self, length: usize) -> MaterialResult<Vec<u8>> {
        let mut out = Vec::with_capacity(length);
        while out.len() < length {
            let chunk = self.next_chunk((length - out.len()) as u64).await?;
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// Refuses anything after the last declared byte.
    async fn expect_end(&mut self) -> MaterialResult<()> {
        loop {
            if !self.pending.is_empty() {
                return Err(MaterialError::Incomplete(
                    "the upload carries more bytes than its manifest declares".to_string(),
                ));
            }
            match self.stream.next().await {
                Some(Ok(chunk)) => self.pending = chunk,
                Some(Err(error)) => {
                    return Err(MaterialError::Incomplete(format!(
                        "the upload could not be read: {error}"
                    )))
                }
                None => return Ok(()),
            }
        }
    }
}
