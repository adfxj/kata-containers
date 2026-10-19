use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::index::SegmentMapping;

pub const ALIGNMENT: u64 = 512;
pub const HEADER_SIZE: u64 = 4096;
pub const BLOCK_SIZE: u64 = 4096;

pub const NO_PHYSICAL_OFFSET: u64 = DiskSegmentMapping::MOFFSET_MASK;

pub const MAGIC0: u64 = 0x00020100544d534c;
pub const MAGIC1: [u8; 16] = [
    0x65, 0x7e, 0x63, 0xd2, 0x94, 0x44, 0x08, 0x4c, 0xa2, 0xd2, 0xc8, 0xec, 0x4f, 0xcf, 0xae,
    0x8a,
];

const FLAG_HEADER: u32 = 1 << 0;
const FLAG_DATA_FILE: u32 = 1 << 1;
const FLAG_SEALED: u32 = 1 << 2;

/// On-disk segment mapping: 16 bytes packed bitfields, compatible with OverlayBD.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiskSegmentMapping {
    pub data_low: u64,
    pub data_high: u64,
}

impl DiskSegmentMapping {
    pub const SIZE: usize = 16;

    const OFFSET_MASK: u64 = (1 << 50) - 1;
    const LENGTH_MASK: u64 = (1 << 14) - 1;
    const MOFFSET_MASK: u64 = (1 << 55) - 1;

    pub fn from_segment_mapping(m: &SegmentMapping) -> Self {
        let offset = m.offset & Self::OFFSET_MASK;
        let length = (m.length as u64) & Self::LENGTH_MASK;
        let low = offset | (length << 50);

        let moffset = m.moffset & Self::MOFFSET_MASK;
        let zeroed = if m.zeroed { 1u64 } else { 0u64 };
        let tag = (m.tag as u64) & 0xFF;
        let high = moffset | (zeroed << 55) | (tag << 56);

        Self {
            data_low: low,
            data_high: high,
        }
    }

    pub fn to_segment_mapping(&self) -> SegmentMapping {
        let offset = self.data_low & Self::OFFSET_MASK;
        let length = ((self.data_low >> 50) & Self::LENGTH_MASK) as u32;
        let moffset = self.data_high & Self::MOFFSET_MASK;
        let zeroed = ((self.data_high >> 55) & 1) != 0;
        let tag = ((self.data_high >> 56) & 0xFF) as u8;
        SegmentMapping::new(offset, length, moffset, zeroed, tag)
    }

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..8].copy_from_slice(&self.data_low.to_le_bytes());
        buf[8..16].copy_from_slice(&self.data_high.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; Self::SIZE]) -> Self {
        Self {
            data_low: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            data_high: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
        }
    }
}

/// On-disk header/trailer: 390 bytes fields, padded to 4096-byte block.
/// Compatible with OverlayBD's LSMT format.
#[derive(Clone, Debug)]
pub struct HeaderTrailer {
    pub magic0: u64,
    pub magic1: [u8; 16],
    pub size: u32,
    pub flags: u32,
    pub index_offset: u64,
    pub index_size: u64,
    pub virtual_size: u64,
    pub uuid: [u8; 37],
    pub parent_uuid: [u8; 37],
    pub reserved: u16,
    pub version: u8,
    pub sub_version: u8,
    pub user_tag: [u8; 256],
}

pub const HEADER_TRAILER_SIZE: usize = 390;

impl Default for HeaderTrailer {
    fn default() -> Self {
        Self {
            magic0: 0,
            magic1: [0; 16],
            size: HEADER_TRAILER_SIZE as u32,
            flags: 0,
            index_offset: 0,
            index_size: 0,
            virtual_size: 0,
            uuid: [0; 37],
            parent_uuid: [0; 37],
            reserved: 0,
            version: 1,
            sub_version: 1,
            user_tag: [0; 256],
        }
    }
}

impl HeaderTrailer {
    pub fn new() -> Self {
        Self {
            magic0: MAGIC0,
            magic1: MAGIC1,
            ..Self::default()
        }
    }

    pub fn verify_magic(&self) -> bool {
        self.magic0 == MAGIC0 && self.magic1 == MAGIC1
    }

    pub fn is_header(&self) -> bool {
        (self.flags & FLAG_HEADER) != 0
    }

    pub fn is_trailer(&self) -> bool {
        !self.is_header()
    }

    pub fn is_data_file(&self) -> bool {
        (self.flags & FLAG_DATA_FILE) != 0
    }

    pub fn is_sealed(&self) -> bool {
        (self.flags & FLAG_SEALED) != 0
    }

    pub fn set_header(&mut self) {
        self.flags |= FLAG_HEADER;
    }

    pub fn set_trailer(&mut self) {
        self.flags &= !FLAG_HEADER;
    }

    pub fn set_data_file(&mut self) {
        self.flags |= FLAG_DATA_FILE;
    }

    pub fn set_sealed(&mut self) {
        self.flags |= FLAG_SEALED;
    }

    pub fn set_uuid(&mut self, uuid: &uuid::Uuid) {
        let s = uuid.hyphenated().to_string();
        let bytes = s.as_bytes();
        self.uuid[..36].copy_from_slice(bytes);
        self.uuid[36] = 0;
    }

    fn to_raw_bytes(&self) -> [u8; HEADER_TRAILER_SIZE] {
        let mut buf = [0u8; HEADER_TRAILER_SIZE];
        buf[0..8].copy_from_slice(&self.magic0.to_le_bytes());
        buf[8..24].copy_from_slice(&self.magic1);
        buf[24..28].copy_from_slice(&self.size.to_le_bytes());
        buf[28..32].copy_from_slice(&self.flags.to_le_bytes());
        buf[32..40].copy_from_slice(&self.index_offset.to_le_bytes());
        buf[40..48].copy_from_slice(&self.index_size.to_le_bytes());
        buf[48..56].copy_from_slice(&self.virtual_size.to_le_bytes());
        buf[56..93].copy_from_slice(&self.uuid);
        buf[93..130].copy_from_slice(&self.parent_uuid);
        buf[130..132].copy_from_slice(&self.reserved.to_le_bytes());
        buf[132] = self.version;
        buf[133] = self.sub_version;
        buf[134..390].copy_from_slice(&self.user_tag);
        buf
    }

    fn from_raw_bytes(buf: &[u8; HEADER_TRAILER_SIZE]) -> Self {
        Self {
            magic0: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            magic1: buf[8..24].try_into().unwrap(),
            size: u32::from_le_bytes(buf[24..28].try_into().unwrap()),
            flags: u32::from_le_bytes(buf[28..32].try_into().unwrap()),
            index_offset: u64::from_le_bytes(buf[32..40].try_into().unwrap()),
            index_size: u64::from_le_bytes(buf[40..48].try_into().unwrap()),
            virtual_size: u64::from_le_bytes(buf[48..56].try_into().unwrap()),
            uuid: buf[56..93].try_into().unwrap(),
            parent_uuid: buf[93..130].try_into().unwrap(),
            reserved: u16::from_le_bytes(buf[130..132].try_into().unwrap()),
            version: buf[132],
            sub_version: buf[133],
            user_tag: buf[134..390].try_into().unwrap(),
        }
    }

    /// Write as a full 4096-byte block (390 bytes + zero padding).
    pub fn write_to(&self, file: &mut File) -> io::Result<()> {
        let mut block = [0u8; BLOCK_SIZE as usize];
        let raw = self.to_raw_bytes();
        block[..HEADER_TRAILER_SIZE].copy_from_slice(&raw);
        file.write_all(&block)?;
        Ok(())
    }

    /// Read a 4096-byte block and parse the header/trailer.
    pub fn read_from(file: &mut File) -> io::Result<Self> {
        let mut block = [0u8; BLOCK_SIZE as usize];
        file.read_exact(&mut block)?;
        let mut raw = [0u8; HEADER_TRAILER_SIZE];
        raw.copy_from_slice(&block[..HEADER_TRAILER_SIZE]);
        let ht = Self::from_raw_bytes(&raw);
        if !ht.verify_magic() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad LSMT magic: {:#x}", ht.magic0),
            ));
        }
        Ok(ht)
    }

    pub fn read_at(file: &mut File, offset: u64) -> io::Result<Self> {
        file.seek(SeekFrom::Start(offset))?;
        Self::read_from(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disk_segment_mapping_size() {
        assert_eq!(DiskSegmentMapping::SIZE, 16);
    }

    #[test]
    fn test_header_trailer_size() {
        assert_eq!(HEADER_TRAILER_SIZE, 390);
    }

    #[test]
    fn test_magic_values() {
        assert_eq!(MAGIC0, 0x00020100544d534c);
        let ht = HeaderTrailer::new();
        assert!(ht.verify_magic());
    }

    #[test]
    fn test_flag_bits() {
        let mut ht = HeaderTrailer::new();
        assert!(!ht.is_header());

        ht.set_header();
        assert!(ht.is_header());
        assert!(!ht.is_trailer());

        ht.set_trailer();
        assert!(!ht.is_header());
        assert!(ht.is_trailer());

        ht.set_data_file();
        assert!(ht.is_data_file());

        ht.set_sealed();
        assert!(ht.is_sealed());
    }

    #[test]
    fn test_disk_segment_mapping_roundtrip() {
        let m = SegmentMapping::new(2048, 8, 16384, false, 7);
        let disk = DiskSegmentMapping::from_segment_mapping(&m);
        assert_eq!(disk.to_segment_mapping(), m);

        let m2 = SegmentMapping::new(2048, 8, 16384, true, 7);
        let disk2 = DiskSegmentMapping::from_segment_mapping(&m2);
        assert_eq!(disk2.to_segment_mapping(), m2);
    }

    #[test]
    fn test_header_trailer_block_roundtrip() {
        let mut ht = HeaderTrailer::new();
        ht.set_header();
        ht.set_data_file();
        ht.virtual_size = 4 * 1024 * 1024 * 1024;
        ht.index_offset = 4096;
        ht.index_size = 100;
        let uuid = uuid::Uuid::new_v4();
        ht.set_uuid(&uuid);

        let raw = ht.to_raw_bytes();
        assert_eq!(raw.len(), 390);

        let ht2 = HeaderTrailer::from_raw_bytes(&raw);
        assert!(ht2.verify_magic());
        assert_eq!(ht2.virtual_size, ht.virtual_size);
        assert_eq!(ht2.index_offset, ht.index_offset);
        assert_eq!(ht2.index_size, ht.index_size);
        assert!(ht2.is_header());
        assert!(ht2.is_data_file());
    }
}
