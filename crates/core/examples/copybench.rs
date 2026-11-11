//! Times one file copy through the operations engine. Used by bench/run.py.
//! usage: copybench SOURCE DEST_FOLDER  (the target must not exist)

use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use gannet_core::ops::{Engine, Event, Request};
use gannet_core::trash::TrashConfig;

fn main() {
    let mut a = std::env::args().skip(1);
    let (src, dest) = (
        PathBuf::from(a.next().expect("source")),
        PathBuf::from(a.next().expect("destination folder")),
    );
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let mut trash = TrashConfig::new(&dest, 0);
    trash.mount_of = Arc::new(|_| None);
    let e = Engine::new(
        trash,
        Arc::new(move |ev| {
            let _ = tx.lock().unwrap().send(ev);
        }),
    );
    let t = Instant::now();
    e.start(Request::Copy {
        sources: vec![src],
        dest,
    });
    loop {
        match rx.recv().unwrap() {
            Event::Finished(o) => {
                assert_eq!(o.errors, 0);
                break;
            }
            Event::Error { message, .. } => panic!("{message}"),
            Event::Conflict(c) => panic!("target exists: {:?}", c.target),
            Event::Progress(_) => {}
        }
    }
    println!("{:.1}", t.elapsed().as_secs_f64() * 1000.0);
}
