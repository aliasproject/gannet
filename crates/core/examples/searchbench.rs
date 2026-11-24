//! Times searching a big tree: how soon the first match comes, how long the whole walk takes,
//! and the longest a cancelled walk can take to stop.
//! usage: searchbench FOLDER [RUNS]
//! An empty FOLDER gets a tree of 200,000 files in 2,220 folders, with "report" in 400 names.

use std::path::Path;
use std::time::{Duration, Instant};

use gannet_core::search::{LIMIT, Step, walk};

fn build(root: &Path) {
    // A folder that already holds something, like /usr, is searched as it is.
    let marker = root.join(".searchbench-tree");
    if marker.exists() || std::fs::read_dir(root).is_ok_and(|mut d| d.next().is_some()) {
        return;
    }
    let mut n = 0;
    for a in 0..20 {
        for b in 0..10 {
            for c in 0..10 {
                let dir = root.join(format!("area-{a}/group-{b}/set-{c}"));
                std::fs::create_dir_all(&dir).unwrap();
                for _ in 0..100 {
                    let name = if n % 500 == 0 { format!("report-{n}.pdf") } else { format!("file-{n}.txt") };
                    std::fs::File::create(dir.join(name)).unwrap();
                    n += 1;
                }
            }
        }
    }
    std::fs::File::create(marker).unwrap();
}

/// (first match, all done, matches, stopped at the limit)
fn time(root: &Path, query: &str) -> (Option<Duration>, Duration, usize, bool) {
    let t = Instant::now();
    let (mut first, mut n) = (None, 0);
    let walked = walk(root, query, false, LIMIT, &mut |s| {
        if let Step::Found(_) = s {
            first.get_or_insert_with(|| t.elapsed());
            n += 1;
        }
        true
    })
    .unwrap()
    .unwrap();
    (first, t.elapsed(), n, walked.truncated)
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn main() {
    let mut a = std::env::args().skip(1);
    let root = std::path::PathBuf::from(a.next().expect("folder"));
    let runs: usize = a.next().map_or(9, |r| r.parse().unwrap());
    build(&root);
    // Warm the page cache, as a folder someone is browsing would be.
    time(&root, "warm");
    println!("{:28} {:>9} {:>9} {:>8}", "query", "first ms", "done ms", "matches");
    for q in ["report", "report-199500", "nothing-matches", "file-1", "e"] {
        let mut firsts = Vec::new();
        let mut dones = Vec::new();
        let mut last = (0, false);
        for _ in 0..runs {
            let (f, d, n, cut) = time(&root, q);
            firsts.extend(f.map(ms));
            dones.push(ms(d));
            last = (n, cut);
        }
        let first = if firsts.is_empty() { "-".to_string() } else { format!("{:.1}", median(firsts)) };
        println!("{:28} {:>9} {:>9.1} {:>8}{}", format!("\"{q}\""), first, median(dones), last.0, if last.1 { " (limit)" } else { "" });
    }
    // A cancelled walk stops at its next tick, so the longest gap between ticks is the longest
    // a cancel can take.
    let mut gaps = Vec::new();
    for _ in 0..runs {
        let mut last = Instant::now();
        let mut worst = Duration::ZERO;
        walk(&root, "nothing-matches", false, LIMIT, &mut |_| {
            worst = worst.max(last.elapsed());
            last = Instant::now();
            true
        })
        .unwrap();
        gaps.push(ms(worst));
    }
    println!("longest a cancel waits: {:.3} ms", median(gaps));
}
