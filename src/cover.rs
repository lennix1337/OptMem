//! Block math. A BLOCK is an aligned power-of-two range of memories, [lo,hi),
//! compressed into one line. Blocks form a binary merge tree over the log:
//! block [lo,hi) is the compression of [lo,mid) and [mid,hi).

pub type Block = (u64, u64);

/// Tile [0,t) with aligned power-of-two blocks; keep a block whole iff its
/// size is at most `alpha` times its age. Bigger alpha = coarser = fewer lines.
fn cover_alpha(t: u64, alpha: f64) -> Vec<Block> {
    let mut root = 1u64;
    while root < t {
        root *= 2;
    }
    let (mut out, mut stack) = (Vec::new(), vec![(0u64, root)]);
    while let Some((lo, hi)) = stack.pop() {
        if lo >= t {
            continue;
        }
        let size = hi - lo;
        if size > 1 && (hi > t || size as f64 > alpha * (t - lo) as f64) {
            let mid = (lo + hi) / 2;
            stack.push((mid, hi));
            stack.push((lo, mid));
        } else {
            out.push((lo, hi));
        }
    }
    out.sort_unstable();
    out
}

/// The blocks `memo wake` prints: at most `budget` of them, finest near t.
/// Detail decays with age, so recent memories stay verbatim and ancient ones
/// collapse. If everything fits, nothing is compressed at all.
pub fn cover(t: u64, budget: u64) -> Vec<Block> {
    if t == 0 {
        return Vec::new();
    }
    if t <= budget {
        return (0..t).map(|i| (i, i + 1)).collect();
    }
    let (mut lo, mut hi) = (0.0f64, 1.0f64);
    for _ in 0..60 {
        let mid = (lo + hi) / 2.0;
        if cover_alpha(t, mid).len() as u64 > budget {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let mut out = cover_alpha(t, hi);
    // Block sizes jump in powers of two, so alpha alone can undershoot the
    // budget. Spend what is left on the present, where detail is worth most.
    while (out.len() as u64) < budget {
        let Some(i) = out.iter().rposition(|b| b.1 - b.0 > 1) else {
            break;
        };
        let (a, b) = out[i];
        let m = (a + b) / 2;
        out.splice(i..=i, [(a, m), (m, b)]);
    }
    out
}

/// Every block buildable from t memories, smallest first: the definition
/// that the store's fast `pending` (which reads level lengths) must agree with.
#[cfg(test)]
pub fn complete(t: u64) -> Vec<Block> {
    let (mut out, mut size) = (Vec::new(), 2);
    while size <= t {
        out.extend((0..t / size).map(|i| (i * size, (i + 1) * size)));
        size *= 2;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const BUDGET: u64 = 96;

    #[test]
    fn cover_tiles_aligned_blocks_with_detail_toward_the_present() {
        let ts = (1..400).chain([1000, 4096, 10000, 65536, 100003]);
        for t in ts {
            let c = cover(t, BUDGET);
            assert!(c.len() as u64 <= BUDGET, "t={t}: over budget");
            assert_eq!((c[0].0, c[c.len() - 1].1), (0, t), "t={t}: does not span");
            for w in c.windows(2) {
                assert_eq!(w[0].1, w[1].0, "t={t}: gap or overlap");
                assert!(
                    w[1].1 - w[1].0 <= w[0].1 - w[0].0,
                    "t={t}: detail decreases"
                );
            }
            for &(lo, hi) in &c {
                let s = hi - lo;
                assert!(
                    s.is_power_of_two() && lo % s == 0,
                    "t={t}: [{lo},{hi}) unaligned"
                );
            }
        }
        assert_eq!(
            cover(300, 320),
            (0..300).map(|i| (i, i + 1)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn every_cover_block_is_buildable() {
        let mut seen = HashSet::new();
        for t in (1..300).chain([512, 700, 1000, 1023, 1024, 2000, 2999]) {
            seen.extend(cover(t, BUDGET).into_iter().filter(|b| b.1 - b.0 > 1));
        }
        let buildable: HashSet<_> = complete(3000).into_iter().collect();
        assert!(seen.is_subset(&buildable));
    }

    #[test]
    fn one_memory_never_creates_a_spike_of_work() {
        let (mut worst, mut prev) = (0, 0);
        for t in 1..2000 {
            let cur = complete(t).len();
            worst = worst.max(cur - prev);
            prev = cur;
        }
        assert!(worst <= 16, "a single memory created {worst} naps");
    }
}
