use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::layer::{LayerStack, LayerWriter};

/// OverlayBD-compatible image config (camelCase JSON).
/// This is what the ublk daemon's `create_image_file` expects to read.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ImageConfig {
    pub repo_blob_url: String,
    pub lowers: Vec<LayerConfig>,
    pub upper: UpperConfig,
    pub result_file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download: Option<DownloadConfig>,
    pub acceleration_layer: bool,
    pub record_trace_path: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LayerConfig {
    pub gzip_index: String,
    pub file: String,
    pub target_file: String,
    pub dir: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub repo_blob_url: String,
    pub digest: String,
    pub uuid: String,
    pub target_digest: String,
    pub size: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UpperConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    pub index: String,
    pub data: String,
    pub target: String,
    pub gzip_index: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DownloadConfig {
    pub enable: bool,
    pub block_size: u64,
    pub concurrency: u32,
    pub max_inflight_blocks: u32,
    pub max_concurrent_files: u32,
}

/// In-memory representation of the memory image layers.
/// Used to build the OverlayBD-compatible image.json.
#[derive(Clone, Debug, Default)]
pub struct MemImageConfig {
    pub virtual_size: u64,
    pub layers: Vec<MemLayerConfig>,
}

#[derive(Clone, Debug, Default)]
pub struct MemLayerConfig {
    pub file: String,
    pub size: u64,
}

impl MemImageConfig {
    /// Save as an OverlayBD-compatible image.json (camelCase).
    pub fn save(&self, path: &Path) -> Result<()> {
        let image_config = self.to_image_config();
        let json = serde_json::to_string_pretty(&image_config)?;
        std::fs::write(path, json)
            .with_context(|| format!("write mem image config: {}", path.display()))?;
        Ok(())
    }

    /// Load from an OverlayBD-compatible image.json.
    pub fn load(path: &Path) -> Result<Self> {
        let json = std::fs::read_to_string(path)
            .with_context(|| format!("read mem image config: {}", path.display()))?;
        let image_config: ImageConfig = serde_json::from_str(&json)
            .context("parse image config json failed")?;
        Ok(Self::from_image_config(&image_config))
    }

    /// Convert to OverlayBD ImageConfig for serialization.
    fn to_image_config(&self) -> ImageConfig {
        let lowers: Vec<LayerConfig> = self
            .layers
            .iter()
            .map(|l| LayerConfig {
                file: l.file.clone(),
                size: l.size,
                ..Default::default()
            })
            .collect();

        ImageConfig {
            repo_blob_url: String::new(),
            lowers,
            upper: UpperConfig::default(),
            result_file: String::new(),
            download: None,
            acceleration_layer: false,
            record_trace_path: String::new(),
        }
    }

    /// Convert from OverlayBD ImageConfig after deserialization.
    fn from_image_config(config: &ImageConfig) -> Self {
        let layers: Vec<MemLayerConfig> = config
            .lowers
            .iter()
            .map(|l| MemLayerConfig {
                file: l.file.clone(),
                size: l.size,
            })
            .collect();

        // virtual_size is not stored in ImageConfig; it's derived from the
        // layer files themselves. Set to 0; the ublk daemon reads it from
        // the LSMT layer header.
        Self {
            virtual_size: 0,
            layers,
        }
    }

    pub fn layer_paths(&self) -> Vec<PathBuf> {
        self.layers.iter().map(|l| PathBuf::from(&l.file)).collect()
    }
}

pub trait DirtyPageSource {
    fn read_page(&self, gpa: u64, buf: &mut [u8]) -> Result<()>;
}

/// Capture dirty pages from a source into a new LSMT layer file.
///
/// `dirty_ranges` is a list of (gpa, length) pairs. Each range should be
/// page-aligned (4K). The data is read from `source` and written into
/// a sealed LSMT layer at `output_path`.
///
/// Returns (file_size, digest).
pub fn capture_dirty_pages(
    source: &dyn DirtyPageSource,
    dirty_ranges: &[(u64, u64)],
    virtual_size: u64,
    output_path: &Path,
) -> Result<(u64, String)> {
    let mut writer = LayerWriter::create(output_path, virtual_size)?;

    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let mut page_buf = vec![0u8; page_size];

    for &(gpa, length) in dirty_ranges {
        let mut offset = gpa;
        let end = gpa + length;

        while offset < end {
            let read_len = (end - offset).min(page_size as u64) as usize;
            let buf_slice = &mut page_buf[..read_len];
            source.read_page(offset, buf_slice)?;
            writer.write_segment(buf_slice, offset)?;
            offset += read_len as u64;
        }
    }

    let _file_size = writer.seal()?;

    let file_size = std::fs::metadata(output_path)?.len();

    Ok((file_size, String::new()))
}

/// Build a MemImageConfig by appending a new layer to the parent's layer list.
pub fn build_mem_image_config(
    parent_config: Option<&MemImageConfig>,
    new_layer_path: &Path,
    new_layer_size: u64,
    virtual_size: u64,
) -> MemImageConfig {
    let mut layers = if let Some(parent) = parent_config {
        parent.layers.clone()
    } else {
        Vec::new()
    };

    layers.push(MemLayerConfig {
        file: new_layer_path.to_string_lossy().to_string(),
        size: new_layer_size,
    });

    let max_layers = 32usize;
    if layers.len() > max_layers {
        tracing::warn!(
            "memory layer count {} exceeds budget {}, compaction needed",
            layers.len(),
            max_layers
        );
    }

    MemImageConfig {
        virtual_size,
        layers,
    }
}

pub fn open_layer_stack(config: &MemImageConfig) -> Result<LayerStack> {
    let paths = config.layer_paths();
    LayerStack::from_paths(&paths)
}
