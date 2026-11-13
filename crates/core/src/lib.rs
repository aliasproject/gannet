//! Everything in Gannet that isn't drawing: folder listings, file operations, the
//! Trash, the system theme, D-Bus. No code here reads the environment; every path comes in
//! as an argument, so tests can't reach real user data by accident.

pub mod bus;
pub mod clipboard;
pub mod disks;
pub mod foldersize;
pub mod history;
pub mod listing;
pub mod mime;
pub mod mounts;
pub mod open;
pub mod ops;
pub mod places;
pub mod recent;
pub mod sort;
pub mod theme;
pub mod thumbcache;
pub mod thumbnailers;
pub mod time;
pub mod trash;
pub mod uri;
pub mod watch;
pub mod webdrop;

#[cfg(test)]
pub(crate) mod testutil;
