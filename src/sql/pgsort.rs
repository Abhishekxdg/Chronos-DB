//! Postgres's own in-memory sorts, over positions: its quicksort (`src/include/lib/sort_template.h`, what
//! tuplesort runs on rows that fit in work_mem) and its incremental sort (`nodeIncrementalSort.c`). Neither is
//! stable, so rows that tie on the sort keys come out in the order Postgres's would leave them in only if they
//! are sorted the same way: these do it step for step.

use std::cmp::Ordering;

/// Sorts `v` as Postgres's qsort does: an insertion sort under 7 elements; else a check whether it's in order
/// already, then a three-way quicksort around the median of three (of three medians of three above 40) that
/// recurses on the smaller side and loops on the larger.
pub(super) fn qsort<T: Copy>(v: &mut [T], cmp: &mut dyn FnMut(T, T) -> Ordering) {
    let n = v.len();
    sort_at(v, 0, n, cmp);
}

/// The median of `v[a]`, `v[b]` and `v[c]`, as Postgres's `med3` picks it.
fn med3<T: Copy>(v: &[T], a: usize, b: usize, c: usize, cmp: &mut dyn FnMut(T, T) -> Ordering) -> usize {
    if cmp(v[a], v[b]) == Ordering::Less {
        if cmp(v[b], v[c]) == Ordering::Less {
            b
        } else if cmp(v[a], v[c]) == Ordering::Less {
            c
        } else {
            a
        }
    } else if cmp(v[b], v[c]) == Ordering::Greater {
        b
    } else if cmp(v[a], v[c]) == Ordering::Less {
        a
    } else {
        c
    }
}

/// Swaps `v[a..a + n]` with `v[b..b + n]`, element by element.
fn swapn<T>(v: &mut [T], a: usize, b: usize, n: usize) {
    for i in 0..n {
        v.swap(a + i, b + i);
    }
}

/// `qsort` of the `n` elements of `v` from `a`.
fn sort_at<T: Copy>(v: &mut [T], mut a: usize, mut n: usize, cmp: &mut dyn FnMut(T, T) -> Ordering) {
    loop {
        if n < 7 {
            for pm in a + 1..a + n {
                let mut pl = pm;
                while pl > a && cmp(v[pl - 1], v[pl]) == Ordering::Greater {
                    v.swap(pl, pl - 1);
                    pl -= 1;
                }
            }
            return;
        }
        let mut presorted = true;
        for pm in a + 1..a + n {
            if cmp(v[pm - 1], v[pm]) == Ordering::Greater {
                presorted = false;
                break;
            }
        }
        if presorted {
            return;
        }
        let mut pm = a + n / 2;
        if n > 7 {
            let mut pl = a;
            let mut pn = a + n - 1;
            if n > 40 {
                let d = n / 8;
                pl = med3(v, pl, pl + d, pl + 2 * d, &mut *cmp);
                pm = med3(v, pm - d, pm, pm + d, &mut *cmp);
                pn = med3(v, pn - 2 * d, pn - d, pn, &mut *cmp);
            }
            pm = med3(v, pl, pm, pn, &mut *cmp);
        }
        v.swap(a, pm);
        let (mut pa, mut pb) = (a + 1, a + 1);
        let (mut pc, mut pd) = (a + n - 1, a + n - 1);
        loop {
            while pb <= pc {
                let r = cmp(v[pb], v[a]);
                if r == Ordering::Greater {
                    break;
                }
                if r == Ordering::Equal {
                    v.swap(pa, pb);
                    pa += 1;
                }
                pb += 1;
            }
            while pb <= pc {
                let r = cmp(v[pc], v[a]);
                if r == Ordering::Less {
                    break;
                }
                if r == Ordering::Equal {
                    v.swap(pc, pd);
                    pd -= 1;
                }
                pc -= 1;
            }
            if pb > pc {
                break;
            }
            v.swap(pb, pc);
            pb += 1;
            pc -= 1;
        }
        let pn = a + n;
        let d1 = (pa - a).min(pb - pa);
        swapn(v, a, pb - d1, d1);
        let d1 = (pd - pc).min(pn - pd - 1);
        swapn(v, pb, pn - d1, d1);
        let (d1, d2) = (pb - pa, pd - pc);
        if d1 <= d2 {
            if d1 > 1 {
                sort_at(v, a, d1, &mut *cmp);
            }
            if d2 > 1 {
                (a, n) = (pn - d2, d2);
                continue;
            }
        } else {
            if d2 > 1 {
                sort_at(v, pn - d2, d2, &mut *cmp);
            }
            if d1 > 1 {
                n = d1;
                continue;
            }
        }
        return;
    }
}

/// How many rows an incremental sort gathers before it looks for the end of a group of rows its input has in
/// order (DEFAULT_MIN_GROUP_SIZE), and past how many in one such group it sorts the group by the other keys alone.
const MIN_GROUP: usize = 32;
const MAX_FULL_SORT_GROUP: usize = 2 * MIN_GROUP;

/// `input`, which is in order by the sort's first keys, sorted by all of them as Postgres's Incremental Sort
/// does it: in batches of at least 32 rows that end where those first keys change, each sorted by all the keys
/// (`full`); a batch whose last group of rows with the same first keys (`same`) passes 64 rows is sorted with
/// the rest of that group alone, by the other keys (`rest`).
pub(super) fn incremental(
    input: &[usize],
    same: &mut dyn FnMut(usize, usize) -> bool,
    full: &mut dyn FnMut(usize, usize) -> Ordering,
    rest: &mut dyn FnMut(usize, usize) -> Ordering,
) -> Vec<usize> {
    let mut out = Vec::with_capacity(input.len());
    let mut it = input.iter().copied();
    // the row that ended the last batch (its group's first): the next batch's first
    let mut carried: Option<usize> = None;
    loop {
        let mut batch: Vec<usize> = carried.take().into_iter().collect();
        // the 32nd row: the batch goes on while rows are in its group
        let mut pivot: Option<usize> = None;
        let (mut done, mut big) = (false, false);
        loop {
            let Some(t) = it.next() else {
                done = true;
                break;
            };
            if batch.len() < MIN_GROUP {
                batch.push(t);
                if batch.len() == MIN_GROUP {
                    pivot = Some(t);
                }
            } else if pivot.is_some_and(|p| same(p, t)) {
                batch.push(t);
            } else {
                carried = Some(t);
                break;
            }
            if batch.len() > MAX_FULL_SORT_GROUP {
                big = true;
                break;
            }
        }
        qsort(&mut batch, &mut *full);
        if !big {
            out.extend(batch);
            if done {
                return out;
            }
            continue;
        }
        // the batch's groups but its last go out as sorted (by the other keys they are in order already); the
        // last takes in the rest of its rows, and is sorted by the other keys
        let mut from = 0;
        loop {
            let first = batch[from];
            let run = batch[from..].iter().take_while(|&&t| same(first, t)).count();
            if from + run == batch.len() {
                break;
            }
            out.extend_from_slice(&batch[from..from + run]);
            from += run;
        }
        let mut group: Vec<usize> = batch[from..].to_vec();
        let last = *group.last().unwrap_or(&batch[from]);
        let mut done = false;
        loop {
            match it.next() {
                None => {
                    done = true;
                    break;
                }
                Some(t) if same(last, t) => group.push(t),
                Some(t) => {
                    carried = Some(t);
                    break;
                }
            }
        }
        qsort(&mut group, &mut *rest);
        out.extend(group);
        if done {
            return out;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{incremental, qsort};

    #[test]
    fn ties_land_where_postgres_leaves_them() {
        // tenk1's rows with unique1 < 10, in heap order (unique1, four): Postgres sorts them by four as
        // 0 8 4 | 5 9 1 | 6 2 | 3 7 (window.out, ORDER BY four)
        let rows = [(4, 0), (2, 2), (1, 1), (6, 2), (9, 1), (8, 0), (5, 1), (3, 3), (7, 3), (0, 0)];
        let mut at: Vec<usize> = (0..rows.len()).collect();
        qsort(&mut at, &mut |a, b| rows[a].1.cmp(&rows[b].1));
        let got: Vec<i32> = at.iter().map(|&i| rows[i].0).collect();
        assert_eq!(got, [0, 8, 4, 5, 9, 1, 6, 2, 3, 7]);
    }

    #[test]
    fn small_and_sorted_inputs_keep_their_order() {
        let keys = [1, 1, 0, 0, 1];
        let mut at: Vec<usize> = (0..5).collect();
        qsort(&mut at, &mut |a, b| keys[a].cmp(&keys[b]));
        assert_eq!(at, [2, 3, 0, 1, 4], "an insertion sort: stable");
        let keys: Vec<i32> = (0..100).map(|i| i / 10).collect();
        let mut at: Vec<usize> = (0..100).collect();
        qsort(&mut at, &mut |a, b| keys[a].cmp(&keys[b]));
        assert_eq!(at, (0..100).collect::<Vec<_>>(), "in order already");
    }

    #[test]
    fn incremental_sorts_small_inputs_whole() {
        // under 32 rows: one sort by every key, as a plain Sort would
        let rows = [(0, 3), (0, 1), (1, 2), (1, 0), (2, 5)];
        let input: Vec<usize> = (0..rows.len()).collect();
        let got =
            incremental(&input, &mut |a, b| rows[a].0 == rows[b].0, &mut |a, b| rows[a].cmp(&rows[b]), &mut |a, b| {
                rows[a].1.cmp(&rows[b].1)
            });
        assert_eq!(got, [1, 0, 3, 2, 4]);
    }
}
