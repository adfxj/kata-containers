use std::cmp::Ordering;

pub const ALIGNMENT: u64 = 512;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Segment {
    pub offset: u64,
    pub length: u32,
}

impl Segment {
    pub fn new(offset: u64, length: u32) -> Self {
        Self { offset, length }
    }

    pub fn end(&self) -> u64 {
        self.offset + self.length as u64
    }

    pub fn contains(&self, offset: u64) -> bool {
        offset >= self.offset && offset < self.end()
    }
}

impl Ord for Segment {
    fn cmp(&self, other: &Self) -> Ordering {
        self.offset.cmp(&other.offset)
    }
}

impl PartialOrd for Segment {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SegmentMapping {
    /// Virtual offset in sector units (offset / ALIGNMENT)
    pub offset: u64,
    /// Length in sector units
    pub length: u32,
    /// Physical offset in sector units (where in the layer file to read)
    pub moffset: u64,
    /// If true, this segment is all zeros (no physical data)
    pub zeroed: bool,
    /// Which layer owns this mapping (index into LayerStack::layers)
    pub tag: u8,
}

impl SegmentMapping {
    pub fn new(offset: u64, length: u32, moffset: u64, zeroed: bool, tag: u8) -> Self {
        Self {
            offset,
            length,
            moffset,
            zeroed,
            tag,
        }
    }

    pub fn end(&self) -> u64 {
        self.offset + self.length as u64
    }

    pub fn segment(&self) -> Segment {
        Segment::new(self.offset, self.length)
    }
}

impl Ord for SegmentMapping {
    fn cmp(&self, other: &Self) -> Ordering {
        self.offset.cmp(&other.offset)
    }
}

impl PartialOrd for SegmentMapping {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SegmentMapping {
    fn eq(&self, other: &Self) -> bool {
        self.offset == other.offset
    }
}

impl Eq for SegmentMapping {}

pub struct ReadOnlyIndex {
    mappings: Vec<SegmentMapping>,
}

impl ReadOnlyIndex {
    pub fn new(mut mappings: Vec<SegmentMapping>) -> Self {
        mappings.sort();
        Self { mappings }
    }

    /// Merge multiple layer mappings into a single index.
    ///
    /// `layers[0]` is the base (oldest) layer, `layers[last]` is the newest.
    /// Newer layers shadow older ones for the same virtual offset range.
    ///
    /// The `tag` field in the resulting mappings is the index into `layers`,
    /// so `self.layers[tag]` gives the correct `LayerReader`.
    pub fn merge(layers: Vec<Vec<SegmentMapping>>) -> Self {
        // Insert all mappings with their layer tag, then sort by (offset, tag).
        // For overlapping ranges, newer (higher tag) shadows older.
        let mut all: Vec<SegmentMapping> = Vec::new();
        for (tag, layer) in layers.into_iter().enumerate() {
            for mut m in layer {
                m.tag = tag as u8;
                all.push(m);
            }
        }
        // Sort by offset, then by tag ascending (older first).
        // This way when we process, newer entries come after older ones
        // for the same offset, and we can let newer shadow older.
        all.sort_by(|a, b| a.offset.cmp(&b.offset).then(a.tag.cmp(&b.tag)));

        // Build merged list: for each mapping, split any existing overlapping
        // older mapping and let the newer one take its place.
        let mut merged: Vec<SegmentMapping> = Vec::new();

        for m in all {
            // Split existing mappings that overlap with m.
            // m is newer (higher tag) than anything already in merged
            // (because we sort by tag ascending for same offset).
            let mut new_entries: Vec<SegmentMapping> = Vec::new();

            for existing in merged.drain(..) {
                if existing.end() <= m.offset || existing.offset >= m.end() {
                    // No overlap — keep existing
                    new_entries.push(existing);
                } else {
                    // Overlap — split existing around m
                    // Keep [existing.offset, m.offset) if non-empty
                    if existing.offset < m.offset {
                        new_entries.push(SegmentMapping {
                            offset: existing.offset,
                            length: (m.offset - existing.offset) as u32,
                            moffset: existing.moffset,
                            zeroed: existing.zeroed,
                            tag: existing.tag,
                        });
                    }
                    // Keep [m.end(), existing.end()) if non-empty
                    if m.end() < existing.end() {
                        let shift = m.end() - existing.offset;
                        new_entries.push(SegmentMapping {
                            offset: m.end(),
                            length: (existing.end() - m.end()) as u32,
                            moffset: if existing.zeroed {
                                0
                            } else {
                                existing.moffset + shift
                            },
                            zeroed: existing.zeroed,
                            tag: existing.tag,
                        });
                    }
                    // The overlapping part [m.offset, m.end) is dropped from existing
                    // — m itself will be added below.
                }
            }

            new_entries.push(m);
            merged = new_entries;
        }

        // Sort by offset for final lookup
        merged.sort();

        Self { mappings: merged }
    }

    pub fn lookup(&self, query: Segment) -> Vec<SegmentMapping> {
        let mut results = Vec::new();

        let pos = self.mappings.partition_point(|m| m.end() <= query.offset);

        for m in &self.mappings[pos..] {
            if m.offset >= query.end() {
                break;
            }

            let overlap_start = m.offset.max(query.offset);
            let overlap_end = m.end().min(query.end());
            let overlap_len = overlap_end - overlap_start;

            if overlap_len == 0 {
                continue;
            }

            let mut clipped = *m;
            if overlap_start != m.offset {
                let shift = overlap_start - m.offset;
                clipped.offset = overlap_start;
                clipped.length = overlap_len as u32;
                if !clipped.zeroed {
                    clipped.moffset += shift;
                }
            } else if overlap_end != m.end() {
                clipped.length = overlap_len as u32;
            }

            results.push(clipped);
        }

        results
    }

    pub fn mappings(&self) -> &[SegmentMapping] {
        &self.mappings
    }

    pub fn len(&self) -> usize {
        self.mappings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.mappings.is_empty()
    }
}
