//! The size Postgres 16's heap would give a table: each row as heap_form_tuple lays it out (a 23-byte header, a
//! null bitmap when a value is null, the values aligned as their types say, short varlena headers where they
//! fit), and pages filled as RelationGetBufferForTuple fills them (8 kB pages of a 24-byte header, a 4-byte line
//! pointer per tuple, fillfactor's free space kept). VACUUM and ANALYZE record what it counts (see `stats`), and
//! the planner reads the pages a table has now from it (estimate_rel_size's curpages).
//!
//! It is a model of a table whose rows were inserted one after another into a fresh heap, in the order Chronos
//! reads them: space a DELETE or UPDATE leaves behind until VACUUM, and the extra pages COPY's bulk extension
//! may add, are not counted. Where a row can't be laid out exactly (a type it doesn't know, a row Postgres
//! would TOAST, a column dropped or added since rows were written), there is no answer, and nothing that needs
//! one uses a guess.
//!
//! Ported from PostgreSQL 16 (src/backend/access/common/heaptuple.c, src/backend/access/heap/hio.c,
//! src/backend/storage/page/bufpage.c, src/backend/utils/adt/numeric.c and geo_ops.c), under the PostgreSQL
//! License:
//!
//! Portions Copyright (c) 1996-2023, PostgreSQL Global Development Group
//! Portions Copyright (c) 1994, The Regents of the University of California
//!
//! Permission to use, copy, modify, and distribute this software and its documentation for any purpose, without
//! fee, and without a written agreement is hereby granted, provided that the above copyright notice and this
//! paragraph and the following two paragraphs appear in all copies.
//!
//! IN NO EVENT SHALL THE UNIVERSITY OF CALIFORNIA BE LIABLE TO ANY PARTY FOR DIRECT, INDIRECT, SPECIAL,
//! INCIDENTAL, OR CONSEQUENTIAL DAMAGES, INCLUDING LOST PROFITS, ARISING OUT OF THE USE OF THIS SOFTWARE AND ITS
//! DOCUMENTATION, EVEN IF THE UNIVERSITY OF CALIFORNIA HAS BEEN ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//!
//! THE UNIVERSITY OF CALIFORNIA SPECIFICALLY DISCLAIMS ANY WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
//! WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE. THE SOFTWARE PROVIDED HEREUNDER IS ON AN "AS
//! IS" BASIS, AND THE UNIVERSITY OF CALIFORNIA HAS NO OBLIGATIONS TO PROVIDE MAINTENANCE, SUPPORT, UPDATES,
//! ENHANCEMENTS, OR MODIFICATIONS.

use super::super::{Table, Type, Value, catalog_schema, geom};

/// BLCKSZ (pg_config.h).
pub(crate) const BLCKSZ: usize = 8192;
/// SizeOfPageHeaderData (bufpage.h): the page header, before the line pointers.
pub(crate) const PAGE_HEADER: usize = 24;
/// sizeof(ItemIdData) (itemid.h): a line pointer.
const ITEM_ID: usize = 4;
/// offsetof(HeapTupleHeaderData, t_bits) (htup_details.h): SizeofHeapTupleHeader.
pub(crate) const TUPLE_HEADER: usize = 23;
/// MaxHeapTupleSize (htup_details.h): BLCKSZ - MAXALIGN(SizeOfPageHeaderData + sizeof(ItemIdData)).
const MAX_TUPLE: usize = BLCKSZ - maxalign(PAGE_HEADER + ITEM_ID);
/// MaxHeapTuplesPerPage (htup_details.h).
const MAX_TUPLES_PER_PAGE: usize = (BLCKSZ - PAGE_HEADER) / (maxalign(TUPLE_HEADER) + ITEM_ID);
/// TOAST_TUPLE_THRESHOLD (heaptoast.h): MaximumBytesPerTuple(TOAST_TUPLES_PER_PAGE = 4). A longer tuple is
/// compressed or moved out of line by heap_toast_insert_or_update, which this doesn't follow.
const TOAST_THRESHOLD: usize = maxalign_down((BLCKSZ - maxalign(PAGE_HEADER + 4 * ITEM_ID)) / 4);
/// VARATT_SHORT_MAX (varatt.h): the most a short (1-byte) varlena header can count, itself included.
const SHORT_MAX: usize = 0x7F;
/// VARHDRSZ (c.h).
const VARHDRSZ: usize = 4;

/// MAXALIGN (c.h), 8 on the 64-bit machines Postgres's expected output comes from.
pub(crate) const fn maxalign(n: usize) -> usize {
    (n + 7) & !7
}

const fn maxalign_down(n: usize) -> usize {
    n & !7
}

/// TYPEALIGN to `a` (1, 2, 4 or 8).
fn align(n: usize, a: usize) -> usize {
    (n + a - 1) & !(a - 1)
}

/// How a column's values are stored: pg_attribute's attlen, attalign and whether a varlena may take a short
/// header (ATT_IS_PACKABLE: attlen -1 and attstorage not 'p'), and for a varlena, how to size its values.
#[derive(Clone, Copy, Debug)]
struct Attr {
    len: i64,
    align: usize,
    packable: bool,
    kind: Kind,
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Fixed,
    /// text, varchar, json, xml: the text's bytes
    Text,
    /// char(n): its value blank-padded to n characters, as bpcharin stores it (None: no length given)
    Bpchar(Option<usize>),
    Bytea,
    Numeric,
    Path,
    Polygon,
}

/// A table's row layout, as far as it can be followed exactly.
#[derive(Clone, Debug)]
pub(crate) struct Layout {
    attrs: Vec<Attr>,
    /// which of the table's columns (in Chronos's order) are the heap's attributes (system columns a read
    /// added aren't)
    cols: Vec<usize>,
    /// RelationGetTargetPageFreeSpace: the space fillfactor keeps free on each page
    save: usize,
}

impl Layout {
    /// `t`'s layout; None where Postgres's can't be known from what Chronos keeps: a column dropped (its
    /// attribute is still in every tuple), rows packed with an older set of columns (ADD COLUMN, whose old
    /// tuples keep fewer attributes), or a column of a type whose stored size this doesn't work out.
    pub(crate) fn of(t: &Table) -> Option<Layout> {
        Layout::filled(t, Fill::Insert)
    }

    /// `of`, for a heap filled as `fill` says: a rewritten one (`Bulk`, `Rewrite`) has every tuple formed anew with
    /// the table's attributes as they are now (reform_and_rewrite_tuple, ATRewriteTable), whatever columns the rows
    /// were written with.
    pub(crate) fn filled(t: &Table, fill: Fill) -> Option<Layout> {
        if !t.schema || !t.dropped.is_empty() || (fill == Fill::Insert && t.layouts.len() > 1) {
            return None;
        }
        let mut attrs = vec![];
        let mut cols = vec![];
        for (j, c) in t.cols.iter().enumerate() {
            if c.hidden {
                continue;
            }
            attrs.push(attr_of(c)?);
            cols.push(j);
        }
        // fillfactor: RelationGetTargetPageFreeSpace (rel.h), BLCKSZ * (100 - fillfactor) / 100
        let fill = t
            .hier
            .reloptions
            .iter()
            .find_map(|o| o.strip_prefix("fillfactor=").and_then(|v| v.parse::<usize>().ok()))
            .unwrap_or(100);
        Some(Layout { attrs, cols, save: BLCKSZ * (100 - fill.min(100)) / 100 })
    }

    /// The stored size of value `v` of the `i`th attribute (not null), and whether it has a short header (and so
    /// no alignment): None for a value this can't size.
    fn datum(&self, i: usize, v: &Value) -> Option<(usize, bool)> {
        let a = self.attrs[i];
        if a.len > 0 {
            return Some((a.len as usize, false));
        }
        // VARSIZE of the value as its input function makes it: a 4-byte header and the payload
        let size = VARHDRSZ
            + match (a.kind, v) {
                (Kind::Text, Value::Text(s) | Value::JsonText(s)) => s.len(),
                (Kind::Bpchar(n), Value::Text(s)) => {
                    let pad = n.map_or(0, |n| n.saturating_sub(s.chars().count()));
                    s.len() + pad
                }
                (Kind::Bytea, Value::Bytea(b)) => b.len(),
                (Kind::Numeric, Value::Numeric(d)) => numeric_size(&d.to_string())?,
                // geo_decls.h: PATH is vl_len_, npts, closed and a dummy int32 before its points; POLYGON is
                // vl_len_, npts and its bounding BOX
                (Kind::Path, Value::Text(s)) => 12 + 16 * geom::parse_path(s).ok()?.pts.len(),
                (Kind::Polygon, Value::Text(s)) => 36 + 16 * geom::parse_polygon(s).ok()?.len(),
                _ => return None,
            };
        // VARATT_CAN_MAKE_SHORT (varatt.h): heap_fill_tuple gives it a 1-byte header if it then fits one
        if a.packable && size - VARHDRSZ < SHORT_MAX {
            return Some((size - VARHDRSZ + 1, true));
        }
        Some((size, false))
    }

    /// t_len of the tuple heap_form_tuple (heaptuple.c) makes of `row` (the table's values, in its column
    /// order): None for one this can't size, or one Postgres would TOAST (longer than TOAST_TUPLE_THRESHOLD).
    pub(crate) fn tuple_len(&self, row: &[Value]) -> Option<usize> {
        let vals: Vec<&Value> = self.cols.iter().map(|&j| row.get(j).unwrap_or(&Value::Null)).collect();
        let nulls = vals.iter().any(|v| matches!(v, Value::Null));
        // offsetof(HeapTupleHeaderData, t_bits), the null bitmap's BITMAPLEN(natts) if there is a null, MAXALIGNed
        let mut hoff = TUPLE_HEADER;
        if nulls {
            hoff += self.attrs.len().div_ceil(8);
        }
        let hoff = maxalign(hoff);
        // heap_compute_data_size (heaptuple.c:216)
        let mut data = 0;
        for (i, v) in vals.iter().enumerate() {
            if matches!(v, Value::Null) {
                continue;
            }
            let (size, short) = self.datum(i, v)?;
            if !short {
                data = align(data, self.attrs[i].align);
            }
            data += size;
        }
        let len = hoff + data;
        (len <= TOAST_THRESHOLD).then_some(len)
    }

    /// ANALYZE's stawidth of the `i`th attribute's value `v` (VARSIZE_ANY of the stored datum for a varlena,
    /// typlen otherwise: analyze.c's compute_*_stats), None if it can't be sized.
    pub(crate) fn width(&self, i: usize, v: &Value) -> Option<usize> {
        self.datum(i, v).map(|(n, _)| n)
    }

    /// The table's columns (in Chronos's order) that are heap attributes, in attribute order.
    pub(crate) fn columns(&self) -> &[usize] {
        &self.cols
    }

    /// Whether the `i`th attribute has a fixed length (its stawidth is its typlen).
    pub(crate) fn fixed(&self, i: usize) -> Option<usize> {
        let a = self.attrs[i];
        (a.len > 0).then_some(a.len as usize)
    }
}

/// Column `c`'s storage facts, from pg_type (and SET STORAGE); None for a type whose values this can't size.
fn attr_of(c: &super::super::Col) -> Option<Attr> {
    if let Type::Enum(_) = c.ty {
        return Some(Attr { len: 4, align: 4, packable: false, kind: Kind::Fixed });
    }
    let (own, _) = super::super::col_typmod(c);
    let oid = if own != 0 { own } else { i64::from(c.ty.oid()) };
    let (len, _, align, storage, _) = catalog_schema::type_facts(oid)?;
    let align = match align.as_str() {
        "c" => 1,
        "s" => 2,
        "i" => 4,
        "d" => 8,
        _ => return None,
    };
    let storage = c.opts.storage.map_or(storage, |s| s.to_string());
    if len > 0 {
        return Some(Attr { len, align, packable: false, kind: Kind::Fixed });
    }
    if len != -1 {
        return None; // a cstring
    }
    let kind = match oid {
        25 | 1043 | 114 | 142 => Kind::Text,
        1042 => Kind::Bpchar(c.chars.map(|(n, _)| n as usize).filter(|&n| n != super::super::VARLEN as usize)),
        17 => Kind::Bytea,
        1700 => Kind::Numeric,
        602 => Kind::Path,
        604 => Kind::Polygon,
        _ => return None,
    };
    Some(Attr { len, align, packable: storage != "p", kind })
}

/// The payload of numeric `text` as make_result (numeric.c) stores it: the short header form's 2 bytes, or the
/// long form's 4, then its base-10000 digits with leading and trailing zero digits dropped, 2 bytes each. NaN and
/// the infinities are the short header alone.
fn numeric_size(text: &str) -> Option<usize> {
    const DEC_DIGITS: usize = 4;
    if matches!(text, "NaN" | "Infinity" | "-Infinity") {
        return Some(2);
    }
    let t = text.strip_prefix('-').unwrap_or(text);
    let (int, frac) = t.split_once('.').unwrap_or((t, ""));
    if int.is_empty() || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let dscale = frac.len();
    // digit groups aligned on the decimal point: the integer part's groups have weights 0, 1, ... from the
    // point leftwards, the fraction's -1, -2, ...
    let int_groups = int.len().div_ceil(DEC_DIGITS);
    let frac_groups = frac.len().div_ceil(DEC_DIGITS);
    let padded_int = format!("{}{int}", "0".repeat(int_groups * DEC_DIGITS - int.len()));
    let padded_frac = format!("{frac}{}", "0".repeat(frac_groups * DEC_DIGITS - frac.len()));
    let digits: Vec<u32> = padded_int
        .as_bytes()
        .chunks(DEC_DIGITS)
        .chain(padded_frac.as_bytes().chunks(DEC_DIGITS))
        .map(|g| g.iter().fold(0, |n, d| n * 10 + u32::from(d - b'0')))
        .collect();
    let mut weight = int_groups as i64 - 1;
    let (mut first, mut n) = (0, digits.len());
    while n > 0 && digits[first] == 0 {
        first += 1;
        weight -= 1;
        n -= 1;
    }
    while n > 0 && digits[first + n - 1] == 0 {
        n -= 1;
    }
    if n == 0 {
        weight = 0;
    }
    // NUMERIC_CAN_BE_SHORT: dscale <= NUMERIC_SHORT_DSCALE_MAX (63), weight in -64..=63
    let short = dscale <= 63 && (-64..=63).contains(&weight);
    Some(if short { 2 } else { 4 } + 2 * n)
}

/// NodesPerPage, NonLeafNodesPerPage and LeafNodesPerPage (fsm_internals.h): a free space map page's binary tree
/// of 1-byte categories, its leaves one per heap page.
const FSM_NODES: usize = BLCKSZ - maxalign(PAGE_HEADER) - 4;
const FSM_NONLEAF: usize = BLCKSZ / 2 - 1;
const FSM_LEAVES: usize = FSM_NODES - FSM_NONLEAF;
/// FSM_CAT_STEP (freespace.c): the bytes one category stands for.
const FSM_CAT_STEP: usize = BLCKSZ / 256;

/// The first page of a heap's free space map (freespace.c, fsmpage.c): the categories RecordAndGetPageWithFreeSpace
/// records for pages an insert left, which a later insert may go back to. Its upper levels are only brought up to
/// date by VACUUM (FreeSpaceMapVacuum), so for a heap filled by inserts alone only this page is searched.
#[derive(Clone, Debug)]
struct Fsm {
    nodes: Vec<u8>,
    next_slot: usize,
}

impl Fsm {
    fn new() -> Fsm {
        Fsm { nodes: vec![0; FSM_NODES], next_slot: 0 }
    }

    /// fsm_space_avail_to_cat (freespace.c:402).
    fn avail_to_cat(avail: usize) -> u8 {
        if avail >= MAX_TUPLE {
            return 255;
        }
        (avail / FSM_CAT_STEP).min(254) as u8
    }

    /// fsm_space_needed_to_cat (freespace.c:442).
    fn needed_to_cat(needed: usize) -> u8 {
        if needed == 0 {
            return 1;
        }
        needed.div_ceil(FSM_CAT_STEP).min(255) as u8
    }

    /// fsm_set_avail (fsmpage.c): the slot's category, propagated up.
    fn set_avail(&mut self, slot: usize, value: u8) {
        let mut nodeno = FSM_NONLEAF + slot;
        if self.nodes[nodeno] == value && value <= self.nodes[0] {
            return;
        }
        self.nodes[nodeno] = value;
        while nodeno > 0 {
            nodeno = (nodeno - 1) / 2;
            let l = 2 * nodeno + 1;
            let mut v = self.nodes[l];
            if l + 1 < FSM_NODES {
                v = v.max(self.nodes[l + 1]);
            }
            if self.nodes[nodeno] == v {
                break;
            }
            self.nodes[nodeno] = v;
        }
    }

    /// fsm_search_avail (fsmpage.c), at the bottom level (advancenext): the first slot from fp_next_slot on
    /// (wrapping around) with at least `minvalue`.
    fn search_avail(&mut self, minvalue: u8) -> Option<usize> {
        if self.nodes[0] < minvalue {
            return None;
        }
        let target = if self.next_slot >= FSM_LEAVES { 0 } else { self.next_slot };
        let mut nodeno = FSM_NONLEAF + target;
        // rightneighbor, then parentof, until a node has enough
        while nodeno > 0 {
            if self.nodes[nodeno] >= minvalue {
                break;
            }
            let mut x = nodeno + 1;
            if ((x + 1) & x) == 0 {
                x = (x - 1) / 2;
            }
            nodeno = (x - 1) / 2;
        }
        // descend, preferring the left child
        while nodeno < FSM_NONLEAF {
            let l = 2 * nodeno + 1;
            if l < FSM_NODES && self.nodes[l] >= minvalue {
                nodeno = l;
            } else if l + 1 < FSM_NODES && self.nodes[l + 1] >= minvalue {
                nodeno = l + 1;
            } else {
                return None; // (a torn page in Postgres, rebuilt there: not reachable here)
            }
        }
        let slot = nodeno - FSM_NONLEAF;
        self.next_slot = slot + 1;
        Some(slot)
    }
}

/// Pages filled as RelationGetBufferForTuple (hio.c:503) fills them for one insert after another (no bulk insert
/// state, as INSERT has): a tuple goes on the page the last one went on (the relcache's target block) if
/// PageGetHeapFreeSpace (bufpage.c) leaves room for it and fillfactor's space; else that page's free space is
/// recorded in the free space map and an earlier page with room is looked for there
/// (RecordAndGetPageWithFreeSpace); else the heap is extended by a page.
#[derive(Clone, Debug)]
pub(crate) struct Pages {
    pub pages: usize,
    /// each page's pd_lower, pd_upper and line pointers
    page: Vec<(usize, usize, usize)>,
    target: Option<usize>,
    fsm: Fsm,
    /// a heap past the free space map's first page, which this doesn't follow
    pub beyond: bool,
    fill: Fill,
}

/// How a heap is filled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Fill {
    /// one insert after another (INSERT's heap_insert, no bulk insert state), with the free space map
    Insert,
    /// in bulk into a new heap, with a bulk insert state and no free space map (TABLE_INSERT_SKIP_FSM):
    /// CREATE TABLE AS's intorel_receive, ALTER TABLE's ATRewriteTable. A tuple goes on the page the last one went on,
    /// else on the next page; the heap is extended (RelationAddBlocks, hio.c:239) by as many pages as it has been
    /// extended by so far, at most 64, so the last pages may be empty (see `Pages::blocks`)
    Bulk,
    /// by VACUUM FULL's and CLUSTER's raw_heap_insert (rewriteheap.c:612): page after page, a new one once a tuple
    /// and fillfactor's space don't fit
    Rewrite,
}

impl Default for Pages {
    fn default() -> Pages {
        Pages::filled(Fill::Insert)
    }
}

impl Pages {
    pub(crate) fn filled(fill: Fill) -> Pages {
        Pages { pages: 0, page: vec![], target: None, fsm: Fsm::new(), beyond: false, fill }
    }

    /// RelationGetNumberOfBlocks of the heap: its pages, and for a bulk fill the empty ones RelationAddBlocks
    /// extended it by past them: it extends by max(1, the pages it extended by so far), at most
    /// MAX_BUFFERS_TO_EXTEND_BY (64), so 1, 1, 2, 4, ... pages, 1, 2, 4, 8, ... 64, 128, 192 in all.
    pub(crate) fn blocks(&self) -> usize {
        if self.fill != Fill::Bulk {
            return self.pages;
        }
        let mut total = 0;
        while total < self.pages {
            total += total.clamp(1, 64);
        }
        total
    }

    /// PageGetHeapFreeSpace (bufpage.c): pd_upper - pd_lower less a line pointer, none once the page has
    /// MaxHeapTuplesPerPage of them.
    fn free(&self, b: usize) -> usize {
        let (lower, upper, lines) = self.page[b];
        let space = (upper - lower).saturating_sub(ITEM_ID);
        if lines >= MAX_TUPLES_PER_PAGE { 0 } else { space }
    }

    fn put(&mut self, b: usize, len: usize) {
        let p = &mut self.page[b];
        p.0 += ITEM_ID;
        p.1 -= len;
        p.2 += 1;
        self.target = Some(b);
    }

    pub(crate) fn add(&mut self, t_len: usize, save: usize) {
        let len = maxalign(t_len);
        // a tuple that with fillfactor's space would leave a page nearly empty only needs the room an empty
        // page has (raw_heap_insert keeps fillfactor's space whatever the tuple)
        let nearly_empty = MAX_TUPLE - (MAX_TUPLES_PER_PAGE / 8 * ITEM_ID);
        let target_free = match self.fill {
            Fill::Insert | Fill::Bulk if len + save > nearly_empty => len.max(nearly_empty),
            _ => len + save,
        };
        // the cached target block; with none (a heap not yet written), the last page
        let mut block = self.target.or(self.pages.checked_sub(1));
        while let Some(b) = block {
            let free = self.free(b);
            if target_free <= free {
                self.put(b, len);
                return;
            }
            // (without the free space map: the next page, a new one here; a bulk fill's next page is one it
            // extended the heap by already, counted in `blocks`)
            if self.fill != Fill::Insert {
                break;
            }
            if b >= FSM_LEAVES {
                self.beyond = true;
                break;
            }
            // RecordAndGetPageWithFreeSpace (freespace.c:154): record this page, search the same FSM page
            self.fsm.set_avail(b, Fsm::avail_to_cat(free));
            block = self.fsm.search_avail(Fsm::needed_to_cat(target_free)).filter(|&s| s < self.pages);
        }
        // RelationAddBlocks: one new page
        self.page.push((PAGE_HEADER, BLCKSZ, 0));
        self.pages += 1;
        self.put(self.pages - 1, len);
    }
}

/// What a read of a table's rows finds of its heap: live tuples and the pages they take, and for each column
/// the sum of its values' stored widths and how many weren't null (ANALYZE's stawidth). None where a row
/// can't be laid out.
#[derive(Clone, Debug, Default)]
pub(crate) struct Count {
    pub tuples: u64,
    pub pages: Pages,
    pub widths: Vec<(u64, u64)>,
}

impl Count {
    pub(crate) fn new(l: &Layout) -> Count {
        Count::filled(l, Fill::Insert)
    }

    /// A count of a heap filled as `fill` says.
    pub(crate) fn filled(l: &Layout, fill: Fill) -> Count {
        Count { widths: vec![(0, 0); l.attrs.len()], pages: Pages::filled(fill), ..Count::default() }
    }

    /// Counts one row; false where it can't be laid out.
    pub(crate) fn add(&mut self, l: &Layout, row: &[Value], widths: bool) -> bool {
        let Some(len) = l.tuple_len(row) else { return false };
        self.tuples += 1;
        self.pages.add(len, l.save);
        if self.pages.beyond {
            return false;
        }
        if widths {
            for (i, &j) in l.cols.iter().enumerate() {
                let v = row.get(j).unwrap_or(&Value::Null);
                if matches!(v, Value::Null) {
                    continue;
                }
                let Some(w) = l.width(i, v) else { return false };
                self.widths[i].0 += w as u64;
                self.widths[i].1 += 1;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_sizes_are_make_results() {
        // 123.45: digits 0123 4500, weight 0, dscale 2: short form
        assert_eq!(numeric_size("123.45"), Some(2 + 4));
        assert_eq!(numeric_size("0"), Some(2));
        assert_eq!(numeric_size("0.00012"), Some(2 + 4));
        assert_eq!(numeric_size("10000"), Some(2 + 2));
        assert_eq!(numeric_size("NaN"), Some(2));
    }

    #[test]
    fn pages_hold_what_postgres_puts_on_them() {
        // a 4-byte tuple of one int: 28 bytes (24 + 4, MAXALIGNed to 32) and a line pointer each
        let mut p = Pages::default();
        for _ in 0..226 {
            p.add(28, 0);
        }
        assert_eq!(p.pages, 1);
        p.add(28, 0);
        assert_eq!(p.pages, 2);
        assert_eq!(MAX_TUPLES_PER_PAGE, 291);
        assert_eq!(TOAST_THRESHOLD, 2032);
    }

    #[test]
    fn a_bulk_fill_extends_by_more_pages_as_it_grows() {
        // 20,000 ints: 89 pages of rows, in a heap extended by 1, 1, 2, 4, 8, 16, 32 and 64 pages
        let mut p = Pages::filled(Fill::Bulk);
        for _ in 0..20_000 {
            p.add(28, 0);
        }
        assert_eq!((p.pages, p.blocks()), (89, 128));
        let mut p = Pages::filled(Fill::Bulk);
        for _ in 0..226 * 2 {
            p.add(28, 0);
        }
        assert_eq!((p.pages, p.blocks()), (2, 2));
        p.add(28, 0);
        assert_eq!((p.pages, p.blocks()), (3, 4));
        let mut p = Pages::filled(Fill::Bulk);
        for _ in 0..226 * 129 {
            p.add(28, 0);
        }
        assert_eq!(p.blocks(), 192);
    }
}
