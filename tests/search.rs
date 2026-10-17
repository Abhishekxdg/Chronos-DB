use chronos::search::{Metric, VecIndex, VecRows};

fn idx() -> VecIndex {
    let mut ix = VecIndex::new(4);
    ix.add(&[1.0, 0.0, 0.0, 0.0], &[("kind", "doc"), ("team", "a")]); // 0
    ix.add(&[0.9, 0.1, 0.0, 0.0], &[("kind", "doc"), ("team", "b")]); // 1
    ix.add(&[0.0, 1.0, 0.0, 0.0], &[("kind", "code"), ("team", "a")]); // 2
    ix.add(&[-1.0, 0.0, 0.0, 0.0], &[("kind", "doc"), ("team", "a")]); // 3
    ix
}

fn ids(r: Vec<(u32, f32)>) -> Vec<u32> {
    r.into_iter().map(|(id, _)| id).collect()
}

#[test]
fn exact_ranks_by_dot_product() {
    assert_eq!(ids(idx().exact(&[1.0, 0.0, 0.0, 0.0], &[], 3)), [0, 1, 2]);
}

#[test]
fn filters_intersect() {
    let ix = idx();
    let q = [1.0, 0.0, 0.0, 0.0];
    assert_eq!(ids(ix.exact(&q, &[("kind", "doc"), ("team", "a")], 10)), [0, 3]);
    assert_eq!(ids(ix.search(&q, &[("team", "b")], 10, 4)), [1]);
    assert!(ix.search(&q, &[("team", "zzz")], 10, 4).is_empty(), "unknown value matches nothing");
}

#[test]
fn quantized_search_matches_exact_when_oversampled() {
    let ix = idx();
    let q = [0.8, 0.3, 0.0, 0.0];
    assert_eq!(ix.search(&q, &[], 2, 2), ix.exact(&q, &[], 2));
}

/// The graph picks its own default beam: the narrowest that finds 99.5% of the scan's top 10 for
/// probe queries near its vectors, and searches at that beam find the true neighbours.
#[test]
fn graph_tunes_its_beam() {
    let (n, dim) = (20_000, 32);
    let mut s = 7u64;
    let mut r = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 11) as f32 / (1u64 << 53) as f32 - 0.5
    };
    let unit = |v: Vec<f32>| {
        let l = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.into_iter().map(|x| x / l).collect::<Vec<f32>>()
    };
    let mut ix = VecIndex::new(dim);
    for _ in 0..n {
        ix.add(&unit((0..dim).map(|_| r()).collect()), &[]);
    }
    ix.build_graph();
    let ef = ix.tuned_ef().expect("tuned once built");
    assert!((64..=400).contains(&ef), "beam {ef}");
    assert!(ef == 400 || ix.graph_recall().unwrap() >= 0.995, "beam {ef}, recall {:?}", ix.graph_recall());
    // new queries at the tuned beam
    let (mut hit, mut total) = (0, 0);
    for _ in 0..50 {
        let q = unit((0..dim).map(|_| r()).collect());
        let truth = ids(ix.exact(&q, &[], 10));
        let got = ids(ix.walk(&q, 10, ef, None, false).unwrap());
        hit += truth.iter().filter(|x| got.contains(x)).count();
        total += truth.len();
    }
    assert!(hit as f32 / total as f32 >= 0.97, "held-out recall {hit}/{total} at beam {ef}");
}

/// The graph's self-test grades it against the exact top 10, not the 1-bit scan. Here every
/// component is positive, so every sign bit is 1 and the scan can't rank at all, while the graph
/// (walked on the vectors below 512 dimensions) finds the true neighbours. Graded against the
/// scan, this good graph scored near 0 and searches would have kept scanning.
#[test]
fn graph_self_test_uses_exact_neighbours() {
    // 70,000 x 64 = 4.48M floats: past EXACT_FLOATS, so the scan ranks by sign bits
    let (n, dim, sub) = (70_000, 64, 6);
    let mut s = 11u64;
    let mut r = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 11) as f32 / (1u64 << 53) as f32 - 0.5
    };
    // points on a low-dimensional patch around the all-ones direction, all components positive
    let basis: Vec<Vec<f32>> = (0..sub).map(|_| (0..dim).map(|_| r()).collect()).collect();
    let mut ix = VecIndex::new(dim);
    for _ in 0..n {
        let z: Vec<f32> = (0..sub).map(|_| r()).collect();
        let v: Vec<f32> = (0..dim).map(|i| 1.0 + 0.3 * (0..sub).map(|j| z[j] * basis[j][i]).sum::<f32>()).collect();
        assert!(v.iter().all(|&x| x > 0.0));
        let l = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        ix.add(&v.iter().map(|x| x / l).collect::<Vec<f32>>(), &[]);
    }
    ix.build_graph();
    let recall = ix.graph_recall().unwrap();
    assert!(recall >= 0.9, "self-test recall {recall}: graded against something other than the true top 10");
}

/// An index moved into place from rows given one at a time is the one `pad_to` and `add` build:
/// the same vectors at the same ids, zero vectors in the gaps (never candidates), the same codes.
#[test]
fn from_rows_matches_add() {
    let dim = 70; // two code words, the second partly used
    let ids = [1u32, 2, 5, 9, 10];
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    };
    let vecs: Vec<Vec<f32>> =
        ids.iter().map(|&id| if id == 5 { vec![0.0; dim] } else { (0..dim).map(|_| next()).collect() }).collect();
    let mut want = VecIndex::new(dim);
    for (&id, v) in ids.iter().zip(&vecs) {
        want.pad_to(id as usize);
        want.add(v, &[]);
    }
    let mut rows = VecRows::new(dim, ids.len());
    for (&id, v) in ids.iter().zip(&vecs) {
        rows.push(id, v);
    }
    let got = VecIndex::from_rows(rows);
    assert_eq!((got.len(), got.unit(), got.zeros()), (want.len(), want.unit(), want.zeros()));
    let all: roaring::RoaringBitmap = (0..got.len() as u32).collect();
    for q in &vecs {
        for id in 0..got.len() as u32 {
            assert_eq!(got.score(id, q), want.score(id, q), "vector {id}");
        }
        assert_eq!(got.exact(q, &[], 11), want.exact(q, &[], 11), "only the rows given are candidates");
        assert_eq!(got.exact_scan(q, &all, 3, 2), want.exact_scan(q, &all, 3, 2), "the same scan");
    }
    let empty = VecIndex::from_rows(VecRows::new(dim, 0));
    assert!(empty.is_empty());
}

/// Big indexes keep vectors at 8 bits a number with a scale each: a score is off by little enough that
/// ranking by it keeps the exact top 10 (here all of them for 1,536-dimension unit vectors),
/// every metric scores through it, and a zero vector stays zero.
#[test]
fn eight_bit_numbers_score_closely_and_rank_the_same() {
    let (n, dim) = (3000, 1536);
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    };
    let unit = |v: Vec<f32>| {
        let l = v.iter().map(|a| a * a).sum::<f32>().sqrt();
        v.into_iter().map(|a| a / l).collect::<Vec<f32>>()
    };
    // clustered, so neighbours are close together, as real embeddings are
    let centres: Vec<Vec<f32>> = (0..30).map(|_| unit((0..dim).map(|_| next()).collect())).collect();
    let vecs: Vec<Vec<f32>> = (0..n)
        .map(|i| unit(centres[i % 30].iter().map(|c| c + 0.4 * next() / (dim as f32).sqrt() * 10.0).collect()))
        .collect();
    let mut ix = VecIndex::eight_bit(dim); // a column this small keeps floats otherwise
    for v in &vecs {
        ix.add(v, &[]);
    }
    let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
    let (mut hit, mut worst) = (0, 0f32);
    for qi in 0..50 {
        let q = unit(vecs[qi * 37].iter().map(|c| c + 0.2 * next() / (dim as f32).sqrt() * 10.0).collect());
        let mut exact: Vec<(u32, f32)> = vecs.iter().enumerate().map(|(i, v)| (i as u32, dot(&q, v))).collect();
        exact.sort_by(|a, b| b.1.total_cmp(&a.1));
        for &(id, s) in exact.iter().take(10) {
            worst = worst.max((ix.score(id, &q) - s).abs());
            let norm = dot(&vecs[id as usize], &vecs[id as usize]).sqrt();
            assert!((ix.score_by(id, &q, Metric::Cosine) - s / norm).abs() < 2e-3, "cosine of {id}");
            let l2 = -vecs[id as usize].iter().zip(&q).map(|(a, b)| (a - b) * (a - b)).sum::<f32>();
            assert!((ix.score_by(id, &q, Metric::L2) - l2).abs() < 4e-3, "L2 of {id}");
        }
        let all: roaring::RoaringBitmap = (0..n as u32).collect();
        let got: Vec<u32> = ix.exact_by(&q, &all, 10, Metric::Dot).into_iter().map(|(i, _)| i).collect();
        hit += exact.iter().take(10).filter(|(i, _)| got.contains(i)).count();
    }
    assert!(worst < 2e-3, "a score off by {worst}");
    assert!(hit >= 495, "{hit} of the true top 500 ranked in by 8-bit numbers");
    let mut z = VecIndex::eight_bit(4);
    z.add(&[0.0; 4], &[]);
    assert_eq!(z.score(0, &[1.0, 0.0, 0.0, 0.0]), 0.0);
    assert_eq!(z.score_by(0, &[1.0, 0.0, 0.0, 0.0], Metric::Cosine), f32::NEG_INFINITY);
}

/// An 8-bit index's exact scan (a small candidate set) returns every row that could be in the
/// true top k, for ranking from the rows: here 200 near-duplicates round to the same numbers but
/// differ as floats, so a cut at a fixed count could drop the true winners; none is dropped.
#[test]
fn eight_bit_exact_scan_keeps_every_row_that_could_win() {
    let (n, dim) = (200, 64);
    let base: Vec<f32> = (0..dim).map(|i| if i % 2 == 0 { 0.125 } else { -0.125 }).collect();
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let mut tiny = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        ((x >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 1e-5
    };
    let vecs: Vec<Vec<f32>> = (0..n).map(|_| base.iter().map(|b| b + tiny()).collect()).collect();
    let mut ix = VecIndex::eight_bit(dim);
    for v in &vecs {
        ix.add(v, &[]);
    }
    let q: Vec<f32> = base.iter().map(|b| b * 8.0).collect(); // |q| = 1
    let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
    let mut truth: Vec<(u32, f32)> = vecs.iter().enumerate().map(|(i, v)| (i as u32, dot(&q, v))).collect();
    truth.sort_by(|a, b| b.1.total_cmp(&a.1));
    let all: roaring::RoaringBitmap = (0..n as u32).collect();
    let got: Vec<u32> = ix.search_in(&q, &all, 5, 64).into_iter().map(|(i, _)| i).collect();
    assert!(truth.iter().take(5).all(|(i, _)| got.contains(i)), "the true top 5 among {} candidates", got.len());
    assert!(got.len() >= n / 2, "near-duplicates that round alike are all candidates ({} of {n})", got.len());
}

/// The same for cosine and L2 over vectors of any length (compared whole, not by dot product):
/// the true top 3 by each metric is always among what an 8-bit index returns. Asking for none
/// returns none.
#[test]
fn eight_bit_exact_by_keeps_every_row_that_could_win_by_any_metric() {
    let (n, dim) = (150, 48);
    let base: Vec<f32> = (0..dim).map(|i| ((i * 7 % 11) as f32 - 5.0) / 10.0).collect();
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut small = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    };
    // lengths from 0.5 to 2.5, directions a hair apart
    let vecs: Vec<Vec<f32>> = (0..n)
        .map(|i| {
            let len = 0.5 + 2.0 * i as f32 / n as f32;
            base.iter().map(|b| (b + 1e-4 * small()) * len).collect()
        })
        .collect();
    let mut ix = VecIndex::eight_bit(dim);
    for v in &vecs {
        ix.add(v, &[]);
    }
    let q: Vec<f32> = {
        let v: Vec<f32> = base.iter().map(|b| b + 0.01 * small()).collect();
        let l = v.iter().map(|a| a * a).sum::<f32>().sqrt();
        v.into_iter().map(|a| a / l).collect()
    };
    let all: roaring::RoaringBitmap = (0..n as u32).collect();
    let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
    for m in [Metric::Cosine, Metric::L2] {
        let exact = |v: &[f32]| match m {
            Metric::Cosine => dot(&q, v) / dot(v, v).sqrt(),
            _ => -v.iter().zip(&q).map(|(a, b)| (a - b) * (a - b)).sum::<f32>(),
        };
        let mut truth: Vec<(u32, f32)> = vecs.iter().enumerate().map(|(i, v)| (i as u32, exact(v))).collect();
        truth.sort_by(|a, b| b.1.total_cmp(&a.1));
        let got: Vec<u32> = ix.exact_by(&q, &all, 3, m).into_iter().map(|(i, _)| i).collect();
        assert!(truth.iter().take(3).all(|(i, _)| got.contains(i)), "{m:?}: the true top 3 among {}", got.len());
        assert!(ix.exact_by(&q, &all, 0, m).is_empty());
    }
    assert!(ix.exact_scan(&q, &all, 0, 64).is_empty());
}

/// A vector of tiny numbers isn't a zero vector (its squared length underflows, its numbers
/// don't): it's ranked from its row like any other, not given a zero vector's score.
#[test]
fn tiny_vectors_are_not_zero_vectors() {
    for mut ix in [VecIndex::new(8), VecIndex::eight_bit(8)] {
        ix.add(&[1e-30; 8], &[]);
        ix.add(&[0.0; 8], &[]);
        assert_eq!(ix.zeros().iter().collect::<Vec<_>>(), [1], "only the all-zero vector");
    }
}

/// L2 over numbers so big their squares overflow f32 (1e20): the 8-bit scores aren't finite, so
/// nothing is dropped before the exact ranking, and the vector equal to the query is among them.
#[test]
fn eight_bit_l2_over_huge_numbers_drops_nothing() {
    let mut ix = VecIndex::eight_bit(4);
    let vecs = [[1e20f32, 0.0, 0.0, 0.0], [0.0, 1e20, 0.0, 0.0], [1e20, 1e20, 0.0, 0.0], [0.0, 0.0, 1e20, 1e20]];
    for v in &vecs {
        ix.add(v, &[]);
    }
    let all: roaring::RoaringBitmap = (0..4).collect();
    let got: Vec<u32> = ix.exact_by(&vecs[2], &all, 1, Metric::L2).into_iter().map(|(i, _)| i).collect();
    assert!(got.contains(&2), "the vector equal to the query: {got:?}");
}
