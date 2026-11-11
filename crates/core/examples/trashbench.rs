//! Times moving a folder's files to Trash through the operations engine. Used by bench/run.py.
//! usage: trashbench FOLDER DATA_HOME  (the Trash is made inside DATA_HOME)

use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use gannet_core::ops::{Engine, Event, Request};
use gannet_core::trash::TrashConfig;

fn main() {
    let mut a = std::env::args().skip(1);
    let (folder, data_home) = (PathBuf::from(a.next().expect("folder")), PathBuf::from(a.next().expect("data home")));
    let paths: Vec<PathBuf> = std::fs::read_dir(&folder).unwrap().map(|e| e.unwrap().path()).collect();
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let e = Engine::new(TrashConfig::new(&data_home, 0), Arc::new(move |ev| {
        let _ = tx.lock().unwrap().send(ev);
    }));
    let t = Instant::now();
    e.start(Request::Trash { paths });
    loop {
        match rx.recv().unwrap() {
            Event::Finished(o) => {
                assert_eq!(o.errors, 0);
                break;
            }
            Event::Error { message, .. } => panic!("{message}"),
            Event::Conflict(c) => panic!("unexpected conflict: {:?}", c.target),
            Event::Progress(_) => {}
        }
    }
    println!("{:.1}", t.elapsed().as_secs_f64() * 1000.0);
}
