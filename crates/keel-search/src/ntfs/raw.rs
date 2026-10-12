//! Where the MFT lies on an NTFS volume, and the raw reading of its records: the part
//! of the index build that needs the volume handle (administrator). The layout comes
//! from record 0 (`$MFT` itself) and, when the MFT is too fragmented for its run list
//! to fit there, from the extension records its `$ATTRIBUTE_LIST` names. Every
//! extent is followed, and a layout this reader does not take (a gap in the run list,
//! a sparse MFT, runs that end before the MFT's size) is an error, so the caller can
//! fall back to enumeration instead of building a short index.
//!
//! Pure apart from the [`RawVolume`] reads, so it is tested over in-memory volume
//! images, and built on every platform.

// Only Windows reads volumes; the tests and the fuzz build use the rest everywhere.
#![allow(dead_code)]

use std::io;

use anyhow::{anyhow, bail, ensure, Context};

use crate::mft::{self, Extents};

/// Bytes of the MFT read per volume read call.
pub(crate) const MFT_CHUNK: u64 = 4 << 20;
/// Most `$MFT` extension records followed (a real MFT needs a handful).
const MAX_EXTENSIONS: usize = 1024;

/// `NTFS_VOLUME_DATA_BUFFER`, the fields the raw MFT read needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct NtfsData {
    pub bytes_per_cluster: u64,
    pub bytes_per_record: u64,
    pub mft_start_lcn: u64,
    /// Bytes of the MFT in use; records past it were never written.
    pub mft_valid_length: u64,
}

/// The reads the raw MFT copy needs: `\\.\X:` opened by an administrator, or an
/// in-memory volume image in tests.
pub(crate) trait RawVolume {
    fn ntfs_data(&self) -> io::Result<NtfsData>;
    /// Fills `buf` from byte `offset` of the volume.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
}

/// Where the MFT's records are on the volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    /// Bytes per record.
    pub record: u64,
    /// Records in use (the `$MFT` size, capped at its valid length).
    pub count: u64,
    /// (byte offset on the volume, records there), in record order; together exactly
    /// `count` records.
    pub extents: Vec<(u64, u64)>,
}

/// Reads record 0 and any `$MFT` extension records and works out where every MFT
/// record lies. Errors on a layout this reader does not take.
pub(crate) fn layout(volume: &dyn RawVolume) -> anyhow::Result<Layout> {
    let data = volume.ntfs_data()?;
    let (cluster, record) = (data.bytes_per_cluster, data.bytes_per_record);
    if cluster == 0
        || !cluster.is_power_of_two()
        || cluster > MFT_CHUNK
        || record < 512
        || record > mft::MAX_RECORD as u64
        || !record.is_power_of_two()
    {
        bail!("unexpected NTFS geometry: {cluster} B clusters, {record} B records");
    }
    let start = data
        .mft_start_lcn
        .checked_mul(cluster)
        .ok_or_else(|| anyhow!("MFT start out of range"))?;
    let mut buf = vec![0u8; cluster.max(record) as usize];
    volume.read_at(start, &mut buf)?;
    let mut zero = buf[..record as usize].to_vec();
    mft::fixup(&mut zero).context("the MFT's first record")?;
    let base = mft::parse_fixed(&zero).context("the MFT's first record")?;
    if base.list_outside {
        bail!("the MFT's attribute list is not inside its first record");
    }
    let (size, mut extents) = mft::data_extents(&zero).context("the MFT's first record")?;
    let size = size.ok_or_else(|| anyhow!("the MFT's first record has no data size"))?;
    // The later extents of a fragmented MFT: in extension records named by the list,
    // each in an extent known by then (NTFS keeps them in the first one).
    let mut named: Vec<&mft::ListEntry> = base
        .list
        .iter()
        .filter(|e| e.kind == mft::ATTR_DATA && !e.named && e.record & mft::RECORD_MASK != 0)
        .collect();
    named.sort_by_key(|e| e.start_vcn);
    let mut read: Vec<u64> = Vec::new();
    for entry in named {
        let number = entry.record & mft::RECORD_MASK;
        if read.contains(&number) {
            continue;
        }
        ensure!(
            read.len() < MAX_EXTENSIONS,
            "too many MFT extension records"
        );
        read.push(number);
        let at = number
            .checked_mul(record)
            .ok_or_else(|| anyhow!("MFT extension record {number} out of range"))?;
        let offset = locate(&extents, cluster, at, record).ok_or_else(|| {
            anyhow!("MFT extension record {number} is outside the extents known before it")
        })?;
        let mut ext = vec![0u8; record as usize];
        volume.read_at(offset, &mut ext)?;
        mft::fixup(&mut ext).with_context(|| format!("MFT extension record {number}"))?;
        let rec =
            mft::parse_fixed(&ext).with_context(|| format!("MFT extension record {number}"))?;
        ensure!(
            rec.in_use
                && rec.base & mft::RECORD_MASK == 0
                && rec.sequence == (entry.record >> 48) as u16,
            "MFT extension record {number} does not belong to the MFT"
        );
        let (_, more) =
            mft::data_extents(&ext).with_context(|| format!("MFT extension record {number}"))?;
        extents.extend(more);
    }
    extents.sort_by_key(|(vcn, _)| *vcn);
    let count = size.min(data.mft_valid_length) / record;
    let mut vcn = 0u64;
    let mut covered = 0u64;
    let mut out = Vec::new();
    for (first, runs) in extents {
        ensure!(
            first == vcn,
            "the MFT's extents leave a gap or overlap at cluster {vcn}"
        );
        for run in runs {
            vcn = vcn
                .checked_add(run.clusters)
                .ok_or_else(|| anyhow!("MFT run out of range"))?;
            if covered >= count {
                continue;
            }
            let lcn = run.lcn.ok_or_else(|| anyhow!("sparse MFT run"))?;
            let offset = lcn
                .checked_mul(cluster)
                .ok_or_else(|| anyhow!("MFT run out of range"))?;
            let bytes = run
                .clusters
                .checked_mul(cluster)
                .ok_or_else(|| anyhow!("MFT run out of range"))?;
            if !bytes.is_multiple_of(record) {
                bail!("an MFT record spans two extents");
            }
            let records = (bytes / record).min(count - covered);
            out.push((offset, records));
            covered += records;
        }
    }
    ensure!(
        covered == count,
        "the MFT's runs cover {covered} of its {count} records"
    );
    Ok(Layout {
        record,
        count,
        extents: out,
    })
}

/// Byte offset on the volume of the `len` bytes at byte `at` of the MFT, when they lie
/// in one known extent.
fn locate(extents: &Extents, cluster: u64, at: u64, len: u64) -> Option<u64> {
    let end = at.checked_add(len)?;
    for (first, runs) in extents {
        let mut lo = first.checked_mul(cluster)?;
        for run in runs {
            let hi = lo.checked_add(run.clusters.checked_mul(cluster)?)?;
            if at >= lo && end <= hi {
                return run.lcn?.checked_mul(cluster)?.checked_add(at - lo);
            }
            lo = hi;
        }
    }
    None
}

/// Reads every record of `layout`, in order, and hands each over with its number
/// (raw: update sequence still applied). Stops at the first error `each` returns.
pub(crate) fn read_records(
    volume: &dyn RawVolume,
    layout: &Layout,
    mut each: impl FnMut(u64, &mut [u8]) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let record = layout.record;
    let mut buf = Vec::new();
    let mut number = 0u64;
    for &(start, records) in &layout.extents {
        let mut offset = start;
        let mut left = records
            .checked_mul(record)
            .ok_or_else(|| anyhow!("MFT extent out of range"))?;
        while left > 0 {
            // A whole number of records: MFT_CHUNK is a multiple of any record size.
            let len = left.min(MFT_CHUNK);
            buf.resize(len as usize, 0);
            volume.read_at(offset, &mut buf)?;
            for chunk in buf.chunks_exact_mut(record as usize) {
                each(number, chunk)?;
                number += 1;
            }
            offset = offset
                .checked_add(len)
                .ok_or_else(|| anyhow!("MFT extent out of range"))?;
            left -= len;
        }
    }
    ensure!(
        number == layout.count,
        "read {number} of {} MFT records",
        layout.count
    );
    Ok(())
}

/// Every file of the MFT as an [`mft::Entry`] (base and extension records merged).
/// Records torn by a write during the read are skipped.
pub(crate) fn read_mft(
    volume: &dyn RawVolume,
    mut each: impl FnMut(mft::Entry),
) -> anyhow::Result<()> {
    let layout = layout(volume)?;
    let mut scan = mft::Scan::default();
    read_records(volume, &layout, |number, chunk| {
        if let Ok(Some(entry)) = scan.add(number, chunk) {
            each(entry);
        }
        Ok(())
    })?;
    for entry in scan.finish() {
        each(entry);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mft::tests::{record, A};
    use crate::mft::{ListEntry, Standard};

    pub(crate) const CLUSTER: u64 = 4096;
    pub(crate) const RECORD: u64 = 1024;
    const IN_USE: u16 = 1;
    const ROOT: u64 = 5 | 5 << 48;
    const MFT_LCN: u64 = 10;

    /// An in-memory volume: a byte image and its geometry. Reads past the end fail.
    pub(crate) struct Image {
        pub data: NtfsData,
        pub bytes: Vec<u8>,
    }

    impl RawVolume for Image {
        fn ntfs_data(&self) -> io::Result<NtfsData> {
            Ok(self.data)
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            let at = usize::try_from(offset).map_err(|_| io::ErrorKind::InvalidInput)?;
            let src = self
                .bytes
                .get(at..at + buf.len())
                .ok_or(io::ErrorKind::UnexpectedEof)?;
            buf.copy_from_slice(src);
            Ok(())
        }
    }

    impl Image {
        /// A volume of `clusters` zeroed clusters with the MFT starting at [`MFT_LCN`].
        pub(crate) fn new(clusters: u64, mft_bytes: u64) -> Self {
            Image {
                data: NtfsData {
                    bytes_per_cluster: CLUSTER,
                    bytes_per_record: RECORD,
                    mft_start_lcn: MFT_LCN,
                    mft_valid_length: mft_bytes,
                },
                bytes: vec![0; (clusters * CLUSTER) as usize],
            }
        }

        /// Writes MFT record `number` where `runs` (lcn, clusters) put it.
        pub(crate) fn put(&mut self, runs: &[(u64, u64)], number: u64, bytes: &[u8]) {
            let mut at = number * RECORD;
            for &(lcn, clusters) in runs {
                let len = clusters * CLUSTER;
                if at < len {
                    let off = (lcn * CLUSTER + at) as usize;
                    self.bytes[off..off + bytes.len()].copy_from_slice(bytes);
                    return;
                }
                at -= len;
            }
            panic!("record {number} is outside the runs");
        }
    }

    /// A mapping-pairs array for absolute (lcn, clusters) runs; lcn None is a hole.
    pub(crate) fn runs(list: &[(Option<u64>, u64)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut last = 0i64;
        for &(lcn, clusters) in list {
            match lcn {
                Some(lcn) => {
                    out.push(0x44);
                    out.extend((clusters as u32).to_le_bytes());
                    out.extend(((lcn as i64 - last) as i32).to_le_bytes());
                    last = lcn as i64;
                }
                None => {
                    out.push(0x04);
                    out.extend((clusters as u32).to_le_bytes());
                }
            }
        }
        out.push(0);
        out
    }

    fn times() -> Standard {
        Standard {
            created: 1,
            modified: 2,
            changed: 3,
            accessed: 4,
            attributes: 0x20,
        }
    }

    /// Record 0: `$MFT` with its first-record runs, `size` bytes, and an optional list.
    pub(crate) fn mft_zero(size: u64, run_bytes: Vec<u8>, list: Option<Vec<ListEntry>>) -> Vec<u8> {
        let mut attrs = vec![A::Standard(times())];
        if let Some(list) = list {
            attrs.push(A::List(list));
        }
        attrs.push(A::Name(ROOT, "$MFT", 3));
        attrs.push(A::NonResident("", 0, size, size, None, 0, run_bytes));
        record(1, 0, IN_USE, &attrs)
    }

    pub(crate) fn file(name: &'static str) -> Vec<u8> {
        record(
            1,
            0,
            IN_USE,
            &[A::Standard(times()), A::Name(ROOT, name, 1), A::Resident(5)],
        )
    }

    fn names(volume: &dyn RawVolume) -> anyhow::Result<Vec<(u64, String)>> {
        let mut out = Vec::new();
        read_mft(volume, |e| {
            let number = e.frn & mft::RECORD_MASK;
            if number >= 16 {
                out.push((number, e.names[0].name.clone()));
            }
        })?;
        out.sort();
        Ok(out)
    }

    /// A `$MFT` too fragmented for record 0: its later extent is in extension record
    /// 20 (named by record 0's attribute list), and files past the first extent are
    /// read too. Without the list, the same layout is refused, not read short.
    #[test]
    fn a_fragmented_mft_is_read_through_its_attribute_list() {
        // 64 records: VCN 0-7 at LCN 10 (records 0-31), VCN 8-15 at LCN 40 (32-63).
        let first = [(MFT_LCN, 8)];
        let all = [(MFT_LCN, 8), (40, 8)];
        let size = 64 * RECORD;
        let mut image = Image::new(48, size);
        let list = vec![
            ListEntry {
                kind: mft::ATTR_DATA,
                start_vcn: 0,
                record: 1 << 48,
                named: false,
            },
            ListEntry {
                kind: mft::ATTR_DATA,
                start_vcn: 8,
                record: 20 | 1 << 48,
                named: false,
            },
        ];
        let zero = mft_zero(size, runs(&[(Some(MFT_LCN), 8)]), Some(list));
        image.put(&all, 0, &zero);
        let ext = record(
            1,
            1 << 48,
            IN_USE,
            &[A::NonResident(
                "",
                8,
                size,
                size,
                None,
                0,
                runs(&[(Some(40), 8)]),
            )],
        );
        image.put(&all, 20, &ext);
        image.put(&all, 30, &file("near.txt"));
        image.put(&all, 40, &file("far.txt"));
        image.put(&all, 63, &file("last.txt"));

        let found = layout(&image).unwrap();
        assert_eq!(found.count, 64);
        assert_eq!(found.extents, [(MFT_LCN * CLUSTER, 32), (40 * CLUSTER, 32)]);
        let got = names(&image).unwrap();
        assert_eq!(
            got,
            [
                (30, "near.txt".to_string()),
                (40, "far.txt".to_string()),
                (63, "last.txt".to_string()),
            ]
        );

        // The same MFT with record 0's list dropped: its runs end at record 31 while
        // its size says 64, so the read is refused instead of short.
        let zero = mft_zero(size, runs(&[(Some(MFT_LCN), 8)]), None);
        image.put(&first, 0, &zero);
        let err = names(&image).unwrap_err().to_string();
        assert!(err.contains("cover 32 of its 64 records"), "{err}");
    }

    /// An extension record that is not the MFT's, or lies outside the extents known,
    /// or a list that leaves a gap: refused.
    #[test]
    fn a_bad_attribute_list_is_refused() {
        let size = 64 * RECORD;
        let all = [(MFT_LCN, 8), (40, 8)];
        let entry = |vcn, rec| ListEntry {
            kind: mft::ATTR_DATA,
            start_vcn: vcn,
            record: rec,
            named: false,
        };
        let build = |list: Vec<ListEntry>, ext_base: u64, ext_vcn: u64| {
            let mut image = Image::new(48, size);
            image.put(
                &all,
                0,
                &mft_zero(size, runs(&[(Some(MFT_LCN), 8)]), Some(list)),
            );
            let ext = record(
                1,
                ext_base,
                IN_USE,
                &[A::NonResident(
                    "",
                    ext_vcn,
                    size,
                    size,
                    None,
                    0,
                    runs(&[(Some(40), 8)]),
                )],
            );
            image.put(&all, 20, &ext);
            image
        };
        let ok = build(vec![entry(8, 20 | 1 << 48)], 1 << 48, 8);
        assert!(layout(&ok).is_ok());
        for (image, says) in [
            // Another file's extension record.
            (
                build(vec![entry(8, 20 | 1 << 48)], 77 | 1 << 48, 8),
                "does not belong",
            ),
            // A stale reference (sequence 2, the record has 1).
            (
                build(vec![entry(8, 20 | 2 << 48)], 1 << 48, 8),
                "does not belong",
            ),
            // Record 40 is in the second extent, unknown until it is read.
            (
                build(vec![entry(8, 40 | 1 << 48)], 1 << 48, 8),
                "outside the extents",
            ),
            // The extension's extent starts at VCN 9: a gap after VCN 8.
            (
                build(vec![entry(9, 20 | 1 << 48)], 1 << 48, 9),
                "gap or overlap",
            ),
        ] {
            let err = format!("{:#}", layout(&image).unwrap_err());
            assert!(err.contains(says), "{err}");
        }
        // A non-resident list in record 0.
        let mut image = Image::new(48, size);
        let zero = record(
            1,
            0,
            IN_USE,
            &[
                A::Standard(times()),
                A::ListOutside,
                A::NonResident("", 0, size, size, None, 0, runs(&[(Some(MFT_LCN), 8)])),
            ],
        );
        image.put(&all, 0, &zero);
        let err = layout(&image).unwrap_err().to_string();
        assert!(err.contains("attribute list"), "{err}");
    }

    /// Record 0 with two runs; the valid length below the size caps the records read;
    /// a torn record is skipped and the rest still come.
    #[test]
    fn multi_run_mft_and_valid_length() {
        let size = 64 * RECORD;
        let all = [(MFT_LCN, 4), (30, 12)];
        // Records past 40 were never written: the valid length stops there.
        let mut image = Image::new(48, 40 * RECORD);
        let run_bytes = runs(&[(Some(MFT_LCN), 4), (Some(30), 12)]);
        image.put(&all, 0, &mft_zero(size, run_bytes, None));
        image.put(&all, 17, &file("a.txt"));
        image.put(&all, 39, &file("b.txt"));
        image.put(&all, 41, &file("beyond.txt"));
        let mut torn = file("torn.txt");
        torn[1022] ^= 0xFF;
        image.put(&all, 18, &torn);
        let found = layout(&image).unwrap();
        assert_eq!(found.count, 40);
        assert_eq!(found.extents, [(MFT_LCN * CLUSTER, 16), (30 * CLUSTER, 24)]);
        let got = names(&image).unwrap();
        assert_eq!(got, [(17, "a.txt".to_string()), (39, "b.txt".to_string())]);
    }

    /// A sparse MFT run, a record split across two extents, an MFT start past the
    /// end of the volume, and impossible geometry: errors, never a short index.
    #[test]
    fn unreadable_layouts_are_errors() {
        let size = 32 * RECORD;
        let check = |image: &Image, says: &str| {
            let err = format!("{:#}", names(image).unwrap_err());
            assert!(err.contains(says), "{err}");
        };
        let mut image = Image::new(48, size);
        let zero = mft_zero(size, runs(&[(Some(MFT_LCN), 4), (None, 4)]), None);
        image.put(&[(MFT_LCN, 4)], 0, &zero);
        check(&image, "sparse MFT run");

        // 512-byte clusters: a 3-cluster run ends inside a 1 KiB record.
        let mut image = Image::new(48, size);
        image.data.bytes_per_cluster = 512;
        image.data.mft_start_lcn = MFT_LCN * 8;
        let zero = mft_zero(size, runs(&[(Some(MFT_LCN * 8), 3), (Some(400), 61)]), None);
        image.bytes[(MFT_LCN * CLUSTER) as usize..][..1024].copy_from_slice(&zero);
        check(&image, "spans two extents");

        // Runs pointing past the end of the volume: the read fails.
        let mut image = Image::new(48, size);
        let zero = mft_zero(size, runs(&[(Some(MFT_LCN), 4), (Some(4_000), 4)]), None);
        image.put(&[(MFT_LCN, 4)], 0, &zero);
        assert!(names(&image).is_err());

        let mut image = Image::new(48, size);
        image.data.mft_start_lcn = 1 << 40;
        assert!(names(&image).is_err());
        for (cluster, record) in [(0, 1024), (3000, 1024), (4096, 256), (4096, 1000)] {
            let mut image = Image::new(48, size);
            image.data.bytes_per_cluster = cluster;
            image.data.bytes_per_record = record;
            check(&image, "unexpected NTFS geometry");
        }
        // Record 0 itself torn.
        let mut image = Image::new(48, size);
        let mut zero = mft_zero(size, runs(&[(Some(MFT_LCN), 8)]), None);
        zero[510] ^= 1;
        image.put(&[(MFT_LCN, 8)], 0, &zero);
        check(&image, "first record");
    }
}
