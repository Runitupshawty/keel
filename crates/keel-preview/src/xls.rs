//! A bound for legacy Excel workbooks (`.xls`) before calamine reads one. calamine reads the
//! whole workbook when it opens it, sized by what the file says: the counts in its compound
//! file header, the chains of its sectors, each sheet's DIMENSIONS record and the corners of
//! its cells (every sheet becomes one dense block from its first cell to its last). A file of
//! a few kilobytes can claim petabytes, and an allocation that fails aborts the process, so
//! catching a panic does not help. [`check`] walks the same structures calamine 0.36 walks,
//! without allocating by them, and says whether calamine stays within one full sheet.

/// The most cells calamine may lay out for a workbook: one full BIFF8 sheet (65,536 rows by
/// 256 columns), summed over every sheet and its formulas.
pub(crate) const MAX_CELLS: u64 = 65_536 * 256;

const SIGNATURE: &[u8] = b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1";
const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
/// Sector ids from here on are markers, not sectors.
const RESERVED: u32 = 0xFFFF_FFFA;
const DIFSECT: u32 = 0xFFFF_FFFC;
/// Streams shorter than this live in the mini stream (calamine's constant).
const MINI_CUTOFF: usize = 4096;

/// Whether calamine can open `file` (the bytes of an .xls) within [`MAX_CELLS`]: false for a
/// file that claims more, or whose structure calamine would follow without bound (a sector
/// chain that loops, a sector past the end of the file).
pub(crate) fn check(file: &[u8]) -> bool {
    workbook_stream(file).is_some_and(|stream| cells(&stream).is_some_and(|n| n <= MAX_CELLS))
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u32s(b: &[u8]) -> impl Iterator<Item = u32> + '_ {
    b.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c))
}

/// Sectors of `size` bytes, sector `id` at `id * size` of `data`. Unlike calamine, a sector
/// that does not lie whole inside the file is refused (calamine pads or shortens it).
struct Sectors<'a> {
    data: &'a [u8],
    size: usize,
}

impl<'a> Sectors<'a> {
    fn get(&self, id: u32) -> Option<&'a [u8]> {
        let start = (id as usize).checked_mul(self.size)?;
        self.data.get(start..start.checked_add(self.size)?)
    }

    /// The chain from `id` through `fat`, `len` bytes of it, as calamine's `get_chain` reads
    /// it (at least one sector, stopping at `len` or the end of the chain). A chain longer
    /// than the sectors there are loops: refused.
    fn chain(&self, mut id: u32, fat: &[u32], len: usize) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let most = self.data.len() / self.size + 1;
        for _ in 0..=most {
            if id == ENDOFCHAIN {
                return Some(out);
            }
            let sector = self.get(id)?;
            out.extend_from_slice(&sector[..sector.len().min(len - out.len())]);
            if out.len() >= len {
                return Some(out);
            }
            id = *fat.get(id as usize)?;
        }
        None
    }
}

/// The workbook stream calamine reads, out of the compound file.
fn workbook_stream(file: &[u8]) -> Option<Vec<u8>> {
    if file.get(..8)? != SIGNATURE || file.len() < 512 {
        return None;
    }
    let large = match u16_at(file, 30)? {
        9 => false,
        12 => true,
        _ => return None,
    };
    let size = if large { 4096 } else { 512 };
    let sectors = Sectors {
        data: file.get(size..)?,
        size,
    };
    let count = file.len() / size + 1;
    // calamine makes room for two counts before it reads a sector: the FAT's (more sectors
    // than the file has is refused) and the DIFAT's, which it reads at offset 62, across
    // the mini FAT's start and length. That one stays under 2^24 (64 MiB reserved) in any
    // file with fewer than 256 mini FAT sectors; more is refused.
    if u32_at(file, 44)? as usize > count || u32_at(file, 62)? >= 1 << 24 {
        return None;
    }
    // The DIFAT: the header's 109 entries, then a chain of sectors whose last entry links on.
    let mut difat: Vec<u32> = u32s(&file[76..512]).collect();
    let mut id = u32_at(file, 68)?;
    let mut steps = 0;
    while id < RESERVED {
        steps += 1;
        if steps > count {
            return None;
        }
        difat.extend(u32s(sectors.get(id)?));
        id = difat.pop()?;
    }
    let fat_sectors: Vec<u32> = difat.into_iter().filter(|id| *id < DIFSECT).collect();
    if fat_sectors.len() > count {
        return None;
    }
    let mut fat = Vec::new();
    for id in fat_sectors {
        fat.extend(u32s(sectors.get(id)?));
    }
    // Directory entries: name (UTF-16, to the first NUL), start sector, stream size.
    let dirs = sectors.chain(u32_at(file, 48)?, &fat, usize::MAX)?;
    let dirs: Vec<(String, u32, usize)> = dirs
        .as_chunks::<128>()
        .0
        .iter()
        .map(|d| {
            let units: Vec<u16> = d[..64]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            let mut name = String::from_utf16_lossy(&units);
            name.truncate(name.find('\0').unwrap_or(name.len()));
            let len = match large {
                true => u64::from_le_bytes(d[120..128].try_into().expect("eight bytes")),
                false => u32_at(d, 120).unwrap_or_default() as u64,
            };
            (name, u32_at(d, 116).unwrap_or_default(), len as usize)
        })
        .collect();
    let root = dirs.first()?;
    let mini_fat_len = u32_at(file, 64)? as usize;
    let (mini_fat, mini_stream) = if mini_fat_len > 0 {
        let stream = sectors.chain(root.1, &fat, root.2)?;
        let mini_fat = sectors.chain(u32_at(file, 60)?, &fat, mini_fat_len.checked_mul(size)?)?;
        (u32s(&mini_fat).collect(), stream)
    } else {
        (Vec::new(), Vec::new())
    };
    let mini = Sectors {
        data: &mini_stream,
        size: 64,
    };
    // calamine reads the first of these names there is (and the next when that fails: one
    // that cannot be read here is refused, as calamine could read it without bound).
    let &(_, start, len) = ["Workbook", "Book", "WORKBOOK", "BOOK"]
        .iter()
        .find_map(|n| dirs.iter().find(|d| d.0 == *n))?;
    match len < MINI_CUTOFF {
        true => mini.chain(start, &mini_fat, len),
        false => sectors.chain(start, &fat, len),
    }
}

/// BIFF records: (type, data), until the stream ends or a record runs past it.
fn records(stream: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    let mut rest = stream;
    std::iter::from_fn(move || {
        let typ = u16_at(rest, 0)?;
        let len = u16_at(rest, 2)? as usize;
        let data = rest.get(4..4 + len)?;
        rest = &rest[4 + len..];
        Some((typ, data))
    })
}

/// The corners of a set of cells.
#[derive(Default)]
struct Extent(Option<(u32, u32, u32, u32)>);

impl Extent {
    fn add(&mut self, row: u32, col: u32) {
        self.0 = Some(match self.0 {
            None => (row, row, col, col),
            Some((r0, r1, c0, c1)) => (r0.min(row), r1.max(row), c0.min(col), c1.max(col)),
        });
    }

    /// The cells of the dense block calamine lays out (`Range::from_sparse`).
    fn cells(&self) -> u64 {
        self.0.map_or(0, |(r0, r1, c0, c1)| {
            (r1 - r0 + 1) as u64 * (c1 - c0 + 1) as u64
        })
    }
}

/// The cells calamine lays out for every sheet of `stream`, or None when one sheet's
/// DIMENSIONS alone asks for more than [`MAX_CELLS`] (or is impossible).
fn cells(stream: &[u8]) -> Option<u64> {
    // Each sheet (BoundSheet8, 0x0085) starts where its record says; the globals end at EOF.
    let sheets: Vec<usize> = records(stream)
        .take_while(|(typ, _)| *typ != 0x000A)
        .filter(|(typ, _)| *typ == 0x0085)
        .map(|(_, data)| u32_at(data, 0).map(|p| p as usize))
        .collect::<Option<_>>()?;
    let mut total = 0u64;
    for pos in sheets {
        let (mut values, mut formulas) = (Extent::default(), Extent::default());
        // A String record is the text value of the formula before it (A1 before any).
        let mut formula_at = (0, 0);
        for (typ, data) in records(stream.get(pos..)?).take_while(|(typ, _)| *typ != 0x000A) {
            let at = || Some((u16_at(data, 0)? as u32, u16_at(data, 2)? as u32));
            match typ {
                // DIMENSIONS: calamine reserves rows x columns cells by it.
                0x0200 => {
                    let (rf, rl, cf, cl) = match data.len() {
                        10 => (
                            u16_at(data, 0)? as u32,
                            u16_at(data, 2)? as u32,
                            u16_at(data, 4)?,
                            u16_at(data, 6)?,
                        ),
                        14 => (
                            u32_at(data, 0)?,
                            u32_at(data, 4)?,
                            u16_at(data, 8)?,
                            u16_at(data, 10)?,
                        ),
                        _ => continue,
                    };
                    let cf = if cf > 0xFF || cl < cf { 0 } else { cf };
                    let (rows, cols) = match rl >= 1 && cl >= 1 {
                        true => (
                            (rl - 1) as i64 - rf as i64 + 1,
                            (cl - 1) as i64 - cf as i64 + 1,
                        ),
                        false => (1, 1),
                    };
                    if rows < 1 || cols < 1 || rows as u64 * cols as u64 > MAX_CELLS {
                        return None;
                    }
                }
                // Number, Label, RString, BoolErr, RK, LabelSst: one cell at (row, column).
                0x0203 | 0x0204 | 0x00D6 | 0x0205 | 0x027E | 0x00FD => {
                    if let Some((r, c)) = at() {
                        values.add(r, c);
                    }
                }
                // Formula: its value and its text.
                0x0006 => {
                    if let Some((r, c)) = at() {
                        values.add(r, c);
                        formulas.add(r, c);
                        formula_at = (r, c);
                    }
                }
                0x0207 => values.add(formula_at.0, formula_at.1),
                // MulRk: a row of cells from the first column, six bytes each.
                0x00BD if data.len() >= 12 => {
                    if let Some((r, c)) = at() {
                        values.add(r, c);
                        values.add(r, c + (data.len() as u32 - 6) / 6 - 1);
                    }
                }
                _ => {}
            }
        }
        total = total
            .saturating_add(values.cells())
            .saturating_add(formulas.cells());
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    #[test]
    fn extents_are_dense_blocks() {
        let mut e = super::Extent::default();
        assert_eq!(e.cells(), 0);
        e.add(5, 3);
        assert_eq!(e.cells(), 1);
        e.add(0, 255);
        e.add(65_535, 0);
        assert_eq!(e.cells(), super::MAX_CELLS);
    }

    /// A DIMENSIONS record claiming exactly the largest sheet passes: calamine reserves it
    /// (not read here, that is 671 MB of address space) and lays out only the cells there are.
    #[cfg(feature = "probes")]
    #[test]
    fn the_largest_sheet_passes() {
        use crate::probes::{biff_dimensions, biff_number, xls};
        let dir = tempfile::tempdir().unwrap();
        let sheet = [
            biff_dimensions((0, 65_536), (0, 256)),
            biff_number(0, 0, 1.0),
            biff_number(65_535, 255, 2.0),
        ]
        .concat();
        for big in [false, true] {
            let path = xls(dir.path(), "max.xls", &sheet, big);
            let bytes = std::fs::read(path).unwrap();
            assert_eq!(
                super::workbook_stream(&bytes).map(|s| super::cells(&s)),
                Some(Some(super::MAX_CELLS))
            );
            assert!(super::check(&bytes));
        }
    }

    #[test]
    fn not_a_compound_file_is_refused() {
        assert!(!super::check(b""));
        assert!(!super::check(&[0u8; 4096]));
    }
}
