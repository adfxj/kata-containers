use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::format::{
    DiskSegmentMapping, HeaderTrailer, ALIGNMENT, BLOCK_SIZE, HEADER_SIZE,
};
use crate::index::{ReadOnlyIndex, Segment, SegmentMapping};

pub struct LayerWriter {
    file: File,
    mappings: Vec<SegmentMapping>,
    virtual_size: u64,
    data_offset: u64,
}

impl LayerWriter {
    pub fn create(path: &Path, virtual_size: u64) -> Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .with_context(|| format!("create layer file: {}", path.display()))?;

        let mut header = HeaderTrailer::new();
        header.set_header();
        header.set_data_file();
        header.virtual_size = virtual_size;
        let uuid = uuid::Uuid::new_v4();
        header.set_uuid(&uuid);
        header.write_to(&mut file)?;

        Ok(Self {
            file,
            mappings: Vec::new(),
            virtual_size,
            data_offset: HEADER_SIZE,
        })
    }

    /// Write a data segment at the given virtual offset.
    /// Both `virt_offset` and `data.len()` should ideally be ALIGNMENT-aligned
    /// for best results. If not, data is padded to alignment boundaries.
    pub fn write_segment(&mut self, data: &[u8], virt_offset: u64) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        // Align down the virtual offset and align up the end
        let aligned_offset = (virt_offset / ALIGNMENT) * ALIGNMENT;
        let padding_front = (virt_offset - aligned_offset) as usize;
        let data_end = virt_offset + data.len() as u64;
        let aligned_end = ((data_end + ALIGNMENT - 1) / ALIGNMENT) * ALIGNMENT;
        let total_len = (aligned_end - aligned_offset) as usize;

        // Build the aligned buffer with padding
        let mut buf = vec![0u8; total_len];
        buf[padding_front..padding_front + data.len()].copy_from_slice(data);

        // Write data at the current append position
        let phys_offset = self.data_offset;
        self.file.seek(SeekFrom::Start(phys_offset))?;
        self.file.write_all(&buf)?;

        // Record mapping in sector units
        let sector_offset = aligned_offset / ALIGNMENT;
        let sector_count = (total_len as u64 / ALIGNMENT) as u32;
        let phys_sector = phys_offset / ALIGNMENT;

        self.mappings.push(SegmentMapping::new(
            sector_offset,
            sector_count,
            phys_sector,
            false,
            0,
        ));

        self.data_offset += total_len as u64;
        Ok(())
    }

    pub fn write_zero_segment(&mut self, virt_offset: u64, length: u64) -> Result<()> {
        if length == 0 {
            return Ok(());
        }
        let aligned_offset = (virt_offset / ALIGNMENT) * ALIGNMENT;
        let aligned_end = ((virt_offset + length + ALIGNMENT - 1) / ALIGNMENT) * ALIGNMENT;
        let sector_offset = aligned_offset / ALIGNMENT;
        let sector_count = ((aligned_end - aligned_offset) / ALIGNMENT) as u32;

        self.mappings.push(SegmentMapping::new(
            sector_offset,
            sector_count,
            0,
            true,
            0,
        ));
        Ok(())
    }

    /// Seal the layer: write the compact index + trailer, then sync.
    /// Returns the file size.
    pub fn seal(mut self) -> Result<u64> {
        self.file.sync_all()?;

        let index_offset = self.data_offset;
        let entry_count = self.mappings.len() as u64;

        // Serialize index: each entry is 16 bytes (DiskSegmentMapping::SIZE)
        let entry_size = DiskSegmentMapping::SIZE;
        let mut index_data = Vec::with_capacity(self.mappings.len() * entry_size);
        for m in &self.mappings {
            let disk = DiskSegmentMapping::from_segment_mapping(m);
            index_data.extend_from_slice(&disk.to_bytes());
        }

        // Pad index to 4096-byte boundary so trailer is block-aligned
        let padded_index_bytes = ((index_data.len() as u64 + BLOCK_SIZE - 1) / BLOCK_SIZE) * BLOCK_SIZE;
        index_data.resize(padded_index_bytes as usize, 0);

        self.file.seek(SeekFrom::Start(index_offset))?;
        self.file.write_all(&index_data)?;

        // Write trailer as a full 4096-byte block at the end
        let trailer_offset = index_offset + padded_index_bytes;

        let mut trailer = HeaderTrailer::new();
        trailer.set_trailer();
        trailer.set_data_file();
        trailer.set_sealed();
        trailer.virtual_size = self.virtual_size;
        trailer.index_offset = index_offset;
        trailer.index_size = entry_count;
        let uuid = uuid::Uuid::new_v4();
        trailer.set_uuid(&uuid);
        self.file.seek(SeekFrom::Start(trailer_offset))?;
        trailer.write_to(&mut self.file)?;

        self.file.sync_all()?;

        let file_size = self.file.metadata()?.len();
        Ok(file_size)
    }
}

pub struct LayerReader {
    file: Arc<File>,
    index: ReadOnlyIndex,
    virtual_size: u64,
    path: PathBuf,
}

impl LayerReader {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .open(path)
            .with_context(|| format!("open layer file: {}", path.display()))?;

        let file_size = file.metadata()?.len();
        if file_size < BLOCK_SIZE {
            bail!("layer file too small: {} ({} bytes)", path.display(), file_size);
        }

        // Read trailer at file_size - BLOCK_SIZE
        let trailer = HeaderTrailer::read_at(&mut file, file_size - BLOCK_SIZE)?;
        if !trailer.is_trailer() {
            bail!("layer file trailer flag not set: {}", path.display());
        }
        if !trailer.is_data_file() {
            bail!("layer file is not a data file: {}", path.display());
        }
        if !trailer.is_sealed() {
            bail!("layer file is not sealed: {}", path.display());
        }

        let virtual_size = trailer.virtual_size;

        // Read index: index_size is the COUNT of entries, not byte size
        let entry_count = trailer.index_size as usize;
        let index_byte_size = entry_count * DiskSegmentMapping::SIZE;
        file.seek(SeekFrom::Start(trailer.index_offset))?;
        let mut index_data = vec![0u8; index_byte_size];
        file.read_exact(&mut index_data)?;

        // Deserialize mappings
        let mut mappings = Vec::with_capacity(entry_count);
        for i in 0..entry_count {
            let offset = i * DiskSegmentMapping::SIZE;
            let mut entry_buf = [0u8; DiskSegmentMapping::SIZE];
            entry_buf.copy_from_slice(&index_data[offset..offset + DiskSegmentMapping::SIZE]);
            let disk = DiskSegmentMapping::from_bytes(&entry_buf);
            mappings.push(disk.to_segment_mapping());
        }

        let index = ReadOnlyIndex::new(mappings);

        Ok(Self {
            file: Arc::new(file),
            index,
            virtual_size,
            path: path.to_path_buf(),
        })
    }

    pub fn virtual_size(&self) -> u64 {
        self.virtual_size
    }

    pub fn index(&self) -> &ReadOnlyIndex {
        &self.index
    }

    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= self.virtual_size || buf.is_empty() {
            return Ok(0);
        }

        // Clamp to virtual size
        let avail = (self.virtual_size - offset).min(buf.len() as u64);
        let read_end = offset + avail;

        let sector_start = offset / ALIGNMENT;
        let sector_end = read_end.div_ceil(ALIGNMENT);
        let query = Segment::new(sector_start, (sector_end - sector_start) as u32);

        let mappings = self.index.lookup(query);

        let mut filled = 0;
        let mut current = offset;

        for m in mappings {
            let m_virt_start = m.offset * ALIGNMENT;
            let m_virt_end = m.end() * ALIGNMENT;

            // Fill hole before this mapping with zeros
            if m_virt_start > current {
                let hole_end = m_virt_start.min(read_end);
                let hole_len = (hole_end - current) as usize;
                if hole_len > 0 {
                    buf[filled..filled + hole_len].fill(0);
                    filled += hole_len;
                    current = hole_end;
                }
            }

            if current >= read_end {
                break;
            }

            let read_start = m_virt_start.max(current);
            let read_end_m = m_virt_end.min(read_end);
            let read_len = (read_end_m - read_start) as usize;
            if read_len == 0 {
                continue;
            }

            if m.zeroed {
                buf[filled..filled + read_len].fill(0);
            } else {
                // Use pread to avoid seeking the shared file fd
                let phys_offset = m.moffset * ALIGNMENT + (read_start - m_virt_start);
                let n = self.file.read_at(&mut buf[filled..filled + read_len], phys_offset)?;
                if n < read_len {
                    // Short read — fill remaining with zeros
                    buf[filled + n..filled + read_len].fill(0);
                }
            }

            filled += read_len;
            current = read_end_m;
        }

        // Fill trailing hole
        if current < read_end {
            let remaining = (read_end - current) as usize;
            buf[filled..filled + remaining].fill(0);
            filled += remaining;
        }

        Ok(filled)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub struct LayerStack {
    layers: Vec<Arc<LayerReader>>,
    merged_index: ReadOnlyIndex,
    virtual_size: u64,
}

impl LayerStack {
    pub fn from_paths(paths: &[PathBuf]) -> Result<Self> {
        if paths.is_empty() {
            bail!("cannot create LayerStack from empty path list");
        }

        let mut layers = Vec::with_capacity(paths.len());
        let mut all_mappings: Vec<Vec<SegmentMapping>> = Vec::new();
        let mut virtual_size = 0u64;

        for path in paths {
            let layer = LayerReader::open(path)?;
            virtual_size = virtual_size.max(layer.virtual_size());
            all_mappings.push(layer.index().mappings().to_vec());
            layers.push(Arc::new(layer));
        }

        let merged_index = ReadOnlyIndex::merge(all_mappings);

        Ok(Self {
            layers,
            merged_index,
            virtual_size,
        })
    }

    pub fn virtual_size(&self) -> u64 {
        self.virtual_size
    }

    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= self.virtual_size || buf.is_empty() {
            return Ok(0);
        }

        // Clamp to virtual size
        let avail = (self.virtual_size - offset).min(buf.len() as u64);
        let read_end = offset + avail;

        let sector_start = offset / ALIGNMENT;
        let sector_end = read_end.div_ceil(ALIGNMENT);
        let query = Segment::new(sector_start, (sector_end - sector_start) as u32);

        let mappings = self.merged_index.lookup(query);

        let mut filled = 0;
        let mut current = offset;

        for m in mappings {
            let m_virt_start = m.offset * ALIGNMENT;
            let m_virt_end = m.end() * ALIGNMENT;

            // Fill hole before this mapping with zeros
            if m_virt_start > current {
                let hole_end = m_virt_start.min(read_end);
                let hole_len = (hole_end - current) as usize;
                if hole_len > 0 {
                    buf[filled..filled + hole_len].fill(0);
                    filled += hole_len;
                    current = hole_end;
                }
            }

            if current >= read_end {
                break;
            }

            let read_start = m_virt_start.max(current);
            let read_end_m = m_virt_end.min(read_end);
            let read_len = (read_end_m - read_start) as usize;
            if read_len == 0 {
                continue;
            }

            if m.zeroed {
                buf[filled..filled + read_len].fill(0);
            } else {
                let tag = m.tag as usize;
                if tag >= self.layers.len() {
                    bail!(
                        "layer tag {} out of range ({} layers)",
                        tag,
                        self.layers.len()
                    );
                }
                let layer = &self.layers[tag];
                let phys_offset = m.moffset * ALIGNMENT + (read_start - m_virt_start);
                let n = layer.file.read_at(&mut buf[filled..filled + read_len], phys_offset)?;
                if n < read_len {
                    buf[filled + n..filled + read_len].fill(0);
                }
            }

            filled += read_len;
            current = read_end_m;
        }

        // Fill trailing hole
        if current < read_end {
            let remaining = (read_end - current) as usize;
            buf[filled..filled + remaining].fill(0);
            filled += remaining;
        }

        Ok(filled)
    }

    /// Get a reference to the merged index (for querying populated extents).
    pub fn merged_index(&self) -> &ReadOnlyIndex {
        &self.merged_index
    }

    /// Clone the LayerStack for multi-threaded reading.
    /// All LayerReaders are behind Arc, so this is cheap (no file re-opening).
    pub fn clone_for_threaded_read(&self) -> Self {
        Self {
            layers: self.layers.clone(),
            merged_index: ReadOnlyIndex::new(self.merged_index.mappings().to_vec()),
            virtual_size: self.virtual_size,
        }
    }
}
