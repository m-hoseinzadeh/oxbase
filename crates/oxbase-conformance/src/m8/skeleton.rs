//! Tells "not written yet" from "broken".
//!
//! The M8 bodies start as `unimplemented!("m8: <stream>")` (`docs/m8-contracts.md` §1), and a
//! stream may land before the ones it calls into (a restore runs the structure-sync compare).
//! Such a panic can happen in a task the services spawn, where the harness only sees an event
//! stream that ends early. So the harness installs a panic hook that records every
//! `not implemented: m8: <stream>` panic (and prints one line instead of the backtrace note);
//! a cell that did not pass while new records appeared is reported as pending, naming the
//! streams. Every other panic goes to the previous hook unchanged.

use std::sync::{Mutex, Once};

static RECORDS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static INSTALL: Once = Once::new();

/// The `<stream>` of an M8 skeleton panic message (`not implemented: m8: <stream>`).
pub fn stream_of(message: &str) -> Option<&str> {
    let rest = message.strip_prefix("not implemented: ")?;
    rest.strip_prefix("m8: ").map(str::trim)
}

fn payload_text(payload: &(dyn std::any::Any + Send)) -> Option<&str> {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
}

/// Installs the recording hook once per process (chained to the hook that was there).
pub fn install() {
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            match payload_text(info.payload()).and_then(stream_of) {
                Some(stream) => {
                    record(stream);
                    eprintln!(
                        "[m8 conformance] reached an unfinished body (m8: {stream}); the cell \
                         reports pending"
                    );
                }
                None => previous(info),
            }
        }));
    });
}

fn record(stream: &str) {
    if let Ok(mut r) = RECORDS.lock() {
        r.push(stream.to_owned());
    }
}

/// A position in the record list; [`since`] returns what came after it.
pub fn mark() -> usize {
    RECORDS.lock().map_or(0, |r| r.len())
}

/// The distinct streams recorded after `mark`, in first-seen order.
pub fn since(mark: usize) -> Vec<String> {
    let Ok(r) = RECORDS.lock() else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for s in r.iter().skip(mark) {
        if !out.contains(s) {
            out.push(s.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_stream_of_a_skeleton_panic() {
        assert_eq!(
            stream_of("not implemented: m8: data-sync"),
            Some("data-sync")
        );
        assert_eq!(stream_of("not implemented"), None);
        assert_eq!(stream_of("not implemented: something else"), None);
        assert_eq!(stream_of("index out of bounds"), None);
    }

    fn unfinished() -> u8 {
        unimplemented!("m8: backup")
    }

    #[test]
    fn records_skeleton_panics_from_tasks() {
        install();
        let mark = mark();
        let joined = std::thread::spawn(unfinished).join();
        assert!(joined.is_err());
        assert_eq!(since(mark), vec!["backup".to_owned()]);
    }
}
