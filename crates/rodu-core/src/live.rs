//! Live sync for long-running servers (`rodu web`, `rodu mcp`): a seam through which they take in
//! teammates' changes and send out their own while they run, without depending on how a team
//! syncs (ADR 0001, step 4c).
//!
//! Ownership and locking: a server holds one `Arc<dyn LiveSync<S>>` and wraps it in a [`Live`].
//! It calls [`Live::pull`] and [`Live::push`] only while it holds its own lock on the service, on
//! a thread that may block (tokio's blocking pool), so sync work is serialized with every request
//! and tool call. An implementation takes `&self`: any state it keeps across calls is its own to
//! synchronize.

use std::sync::{Arc, Mutex};

use crate::{Result, RoduService, Store};

/// What a pull did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pulled {
    /// Whether the board may look different now: something was imported or numbered.
    pub changed: bool,
    /// Problems worth telling the user that did not stop the pull, such as a skipped file.
    pub warnings: Vec<String>,
}

/// Takes in teammates' changes and sends out this replica's, such as through a team folder.
pub trait LiveSync<S: Store>: Send + Sync {
    /// Takes in what teammates wrote. An `Err` means nothing could be taken in this time.
    fn pull(&self, service: &RoduService<S>) -> Result<Pulled>;
    /// Sends out this replica's changes; returns warnings that did not stop it.
    fn push(&self, service: &RoduService<S>) -> Result<Vec<String>>;
}

/// A [`LiveSync`] as a server runs it: problems never fail a request or a tool call. They go to
/// stderr (never stdout, which is MCP's protocol channel) as `warning: sync: ...`, and a message
/// is written only when it was not already written by the call before, so a folder that stays
/// unreachable is reported once, not every few seconds.
pub struct Live<S: Store> {
    sync: Arc<dyn LiveSync<S>>,
    last: Mutex<Vec<String>>,
    write: Box<dyn Fn(&str) + Send + Sync>,
}

impl<S: Store> Live<S> {
    pub fn new(sync: Arc<dyn LiveSync<S>>) -> Self {
        Self::writing_to(sync, |line| eprintln!("{line}"))
    }

    /// For tests: where the warnings go.
    #[doc(hidden)]
    pub fn writing_to(
        sync: Arc<dyn LiveSync<S>>,
        write: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        Self { sync, last: Mutex::new(Vec::new()), write: Box::new(write) }
    }

    /// Pulls; returns whether anything changed.
    pub fn pull(&self, service: &RoduService<S>) -> bool {
        match self.sync.pull(service) {
            Ok(pulled) => {
                self.report("pull", &pulled.warnings);
                pulled.changed
            }
            Err(e) => {
                self.report("pull", &[format!("{} (working offline)", e.message)]);
                false
            }
        }
    }

    pub fn push(&self, service: &RoduService<S>) {
        match self.sync.push(service) {
            Ok(warnings) => self.report("push", &warnings),
            Err(e) => self.report(
                "push",
                &[format!("{} (your changes stay here and go out next time)", e.message)],
            ),
        }
    }

    /// Writes the lines of `step` that differ from what that step said last time.
    fn report(&self, step: &str, lines: &[String]) {
        let tagged: Vec<String> = lines.iter().map(|l| format!("{step}\u{0}{l}")).collect();
        let Ok(mut last) = self.last.lock() else { return };
        let fresh: Vec<&String> = lines
            .iter()
            .zip(&tagged)
            .filter(|(_, tag)| !last.contains(tag))
            .map(|(line, _)| line)
            .collect();
        for line in fresh {
            (self.write)(&format!("warning: sync: {line}"));
        }
        last.retain(|tag| !tag.starts_with(&format!("{step}\u{0}")));
        last.extend(tagged);
    }
}
