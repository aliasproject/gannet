//! Natural ordering: case-insensitive, digit runs compare as numbers. It is by code
//! point, not locale collation, which is much faster and stable across locales.

use std::cmp::Ordering;

/// Compares two file names. Equal only if the bytes are equal, so sorts are total.
pub fn natural_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        if x.is_ascii_digit() && y.is_ascii_digit() {
            let (ie, je) = (digit_run_end(a, i), digit_run_end(b, j));
            let (ra, rb) = (trim_zeros(&a[i..ie]), trim_zeros(&b[j..je]));
            let ord = ra.len().cmp(&rb.len()).then_with(|| ra.cmp(rb));
            if ord != Ordering::Equal {
                return ord;
            }
            i = ie;
            j = je;
        } else if x < 0x80 && y < 0x80 {
            let ord = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
            if ord != Ordering::Equal {
                return ord;
            }
            i += 1;
            j += 1;
        } else {
            let (cx, nx) = decode(&a[i..]);
            let (cy, ny) = decode(&b[j..]);
            let ord = fold(cx).cmp(&fold(cy));
            if ord != Ordering::Equal {
                return ord;
            }
            i += nx;
            j += ny;
        }
    }
    (a.len() - i).cmp(&(b.len() - j)).then_with(|| a.cmp(b))
}

fn digit_run_end(s: &[u8], start: usize) -> usize {
    let mut e = start;
    while e < s.len() && s[e].is_ascii_digit() {
        e += 1;
    }
    e
}

fn trim_zeros(s: &[u8]) -> &[u8] {
    let n = s.iter().take_while(|&&c| c == b'0').count();
    &s[n..]
}

/// Decodes one character. An invalid byte maps to a private-use code point so names
/// that aren't UTF-8 still sort consistently.
fn decode(s: &[u8]) -> (char, usize) {
    let len = match s[0] {
        0..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    };
    if len <= s.len()
        && let Ok(t) = std::str::from_utf8(&s[..len])
        && let Some(c) = t.chars().next()
    {
        return (c, len);
    }
    (char::from_u32(0xE000 + s[0] as u32).unwrap_or('\u{FFFD}'), 1)
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Sorts in parallel when the input is large enough to be worth the threads.
pub fn par_sort_by<T, F>(v: &mut Vec<T>, cmp: F)
where
    T: Copy + Send + Sync,
    F: Fn(&T, &T) -> Ordering + Sync,
{
    const PARALLEL_FROM: usize = 16 * 1024;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8);
    if v.len() < PARALLEL_FROM || threads < 2 {
        v.sort_unstable_by(&cmp);
        return;
    }
    let chunk = v.len().div_ceil(threads);
    std::thread::scope(|s| {
        for part in v.chunks_mut(chunk) {
            let cmp = &cmp;
            s.spawn(move || part.sort_unstable_by(cmp));
        }
    });
    let mut width = chunk;
    let mut src = std::mem::take(v);
    let mut dst: Vec<T> = Vec::with_capacity(src.len());
    while width < src.len() {
        dst.clear();
        dst.extend_from_slice(&src);
        {
            // Each pass merges neighbouring runs of `width` from src into dst.
            let cmp = &cmp;
            std::thread::scope(|s| {
                for (from, to) in src.chunks(width * 2).zip(dst.chunks_mut(width * 2)) {
                    s.spawn(move || merge(from, width.min(from.len()), to, cmp));
                }
            });
        }
        std::mem::swap(&mut src, &mut dst);
        width *= 2;
    }
    *v = src;
}

fn merge<T: Copy, F: Fn(&T, &T) -> Ordering>(from: &[T], mid: usize, to: &mut [T], cmp: &F) {
    let (a, b) = from.split_at(mid);
    let (mut i, mut j) = (0, 0);
    for slot in to.iter_mut() {
        let take_a = j >= b.len() || (i < a.len() && cmp(&a[i], &b[j]) != Ordering::Greater);
        if take_a {
            *slot = a[i];
            i += 1;
        } else {
            *slot = b[j];
            j += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(mut v: Vec<&str>) -> Vec<&str> {
        v.sort_by(|a, b| natural_cmp(a.as_bytes(), b.as_bytes()));
        v
    }

    #[test]
    fn digit_runs_compare_as_numbers() {
        assert_eq!(sorted(vec!["a10", "a9", "a100", "a1"]), ["a1", "a9", "a10", "a100"]);
        assert_eq!(sorted(vec!["x007", "x7", "x06"]), ["x06", "x007", "x7"]);
    }

    #[test]
    fn case_is_ignored_then_bytes_break_ties() {
        assert_eq!(sorted(vec!["b", "A", "a", "B"]), ["A", "a", "B", "b"]);
        assert_eq!(sorted(vec!["École", "eagle", "zebra"]), ["eagle", "zebra", "École"]);
        assert_eq!(natural_cmp("É".as_bytes(), "é".as_bytes()), "É".as_bytes().cmp("é".as_bytes()));
    }

    #[test]
    fn huge_digit_runs_dont_overflow() {
        let a = "9".repeat(400);
        let b = format!("1{}", "0".repeat(400));
        assert_eq!(natural_cmp(a.as_bytes(), b.as_bytes()), Ordering::Less);
    }

    #[test]
    fn invalid_utf8_sorts_without_panicking() {
        let names: [&[u8]; 4] = [b"\xff\xfe", b"a\x80b", b"a", b"\xc3"];
        for x in names {
            for y in names {
                assert_eq!(natural_cmp(x, y) == Ordering::Equal, x == y);
            }
        }
    }

    #[test]
    fn parallel_sort_matches_plain_sort() {
        let mut state = 12345u64;
        let v: Vec<u64> = (0..100_000)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                state >> 40
            })
            .collect();
        let mut a = v.clone();
        let mut b = v;
        a.sort_unstable();
        par_sort_by(&mut b, |x, y| x.cmp(y));
        assert_eq!(a, b);
    }
}
