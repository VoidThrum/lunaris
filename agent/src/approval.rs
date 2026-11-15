//! Asking the person at this computer to accept a connection made without
//! the access password, like RustDesk's "accept sessions via click" and
//! AnyDesk's accept window.
//!
//! The agent has no window of its own when the app runs it, so the app shows
//! the question: started with `--approval-stdio`, the agent writes each
//! question as one line on stdout and reads the answers from stdin:
//!
//! ```text
//! agent -> app   @lunaris-approval {"id":1,"requester":"huy (server x)","kind":"control","timeout_s":30}
//! agent -> app   @lunaris-approval-cancel {"id":1}
//! app -> agent   {"id":1,"accept":true}
//! ```
//!
//! Without a window to ask in ("unattended"), only the password lets anyone in.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tracing::{info, warn};

/// How long the question stays up before it counts as declined.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// How connections without the right password are treated (`approve_mode`
/// in the config), the same choices as RustDesk's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApproveMode {
    /// Only the password lets anyone in.
    Password,
    /// Every connection is accepted at this computer, password or not.
    Click,
    /// The password lets one in; without it, the person here can accept.
    PasswordClick,
}

impl ApproveMode {
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "password" => ApproveMode::Password,
            "click" => ApproveMode::Click,
            _ => ApproveMode::PasswordClick,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// See and control the screen.
    Control,
    /// Transfer files.
    Files,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Accepted,
    Denied,
    Timeout,
    /// Nobody can answer here.
    Unattended,
}

impl Decision {
    /// `status` of `SignalingMessage::AccessRequest`.
    pub fn status(self) -> &'static str {
        match self {
            Decision::Accepted => "accepted",
            Decision::Denied => "denied",
            Decision::Timeout => "timeout",
            Decision::Unattended => "unattended",
        }
    }
}

#[derive(Serialize)]
struct Question<'a> {
    id: u64,
    requester: &'a str,
    kind: &'a str,
    timeout_s: u64,
}

#[derive(Deserialize)]
struct Answer {
    id: u64,
    accept: bool,
}

static ATTACHED: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static PENDING: Mutex<Option<HashMap<u64, oneshot::Sender<bool>>>> = Mutex::new(None);
static STDOUT: Mutex<()> = Mutex::new(());

/// Lets the app answer: questions on stdout, answers on stdin.
pub fn attach_stdio() {
    if ATTACHED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::Builder::new()
        .name("approval-stdin".into())
        .spawn(|| {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                match serde_json::from_str::<Answer>(line.trim()) {
                    Ok(a) => answer(a.id, a.accept),
                    Err(_) if line.trim().is_empty() => {}
                    Err(e) => warn!("approval: ignoring {:?}: {}", line, e),
                }
            }
            // The app is gone: nobody can answer any more.
            ATTACHED.store(false, Ordering::SeqCst);
            if let Some(map) = PENDING.lock().unwrap().as_mut() {
                for (_, tx) in map.drain() {
                    let _ = tx.send(false);
                }
            }
        })
        .ok();
}

/// Whether someone can be asked.
pub fn attended() -> bool {
    ATTACHED.load(Ordering::SeqCst)
}

fn emit(line: &str) {
    let _guard = STDOUT.lock().unwrap();
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", line);
    let _ = out.flush();
}

fn answer(id: u64, accept: bool) {
    let tx = PENDING.lock().unwrap().as_mut().and_then(|m| m.remove(&id));
    if let Some(tx) = tx {
        let _ = tx.send(accept);
    }
}

/// What the person here is asked about for a signaling message; None for
/// messages that need no permission.
pub fn kind_of(sig: &common::SignalingMessage) -> Option<Kind> {
    use common::SignalingMessage::*;
    match sig {
        RequestSession { .. } | IncomingSession { .. } => Some(Kind::Control),
        RequestFileSession { .. } | IncomingFileSession { .. } => Some(Kind::Files),
        _ => None,
    }
}

/// Messages a client may send before it is accepted: questions about the
/// computer, and the end of its own connection; nothing that shows or
/// changes anything.
pub fn allowed_before_approval(sig: &common::SignalingMessage) -> bool {
    use common::SignalingMessage::*;
    matches!(sig, GetCapabilities { .. } | GetAppList { .. } | EndSession { .. })
}

/// Asks the person here to let `requester` in; waits up to [`TIMEOUT`].
pub async fn ask(requester: &str, kind: Kind) -> Decision {
    ask_unless(requester, kind, std::future::pending::<()>()).await.unwrap_or(Decision::Denied)
}

/// Like [`ask`], but withdrawn when `cancel` completes first (the one
/// asking went away): the question is taken down and None returned.
pub async fn ask_unless(requester: &str, kind: Kind, cancel: impl std::future::Future<Output = ()>) -> Option<Decision> {
    if !attended() {
        return Some(Decision::Unattended);
    }
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();
    PENDING.lock().unwrap().get_or_insert_with(HashMap::new).insert(id, tx);
    let question = Question {
        id,
        requester,
        kind: match kind {
            Kind::Control => "control",
            Kind::Files => "files",
        },
        timeout_s: TIMEOUT.as_secs(),
    };
    info!("Asking to accept a connection from {:?} ({:?})", requester, kind);
    emit(&format!("@lunaris-approval {}", serde_json::to_string(&question).unwrap_or_default()));
    let decision = tokio::select! {
        answer = tokio::time::timeout(TIMEOUT, rx) => Some(match answer {
            Ok(Ok(true)) => Decision::Accepted,
            Ok(_) => Decision::Denied,
            Err(_) => Decision::Timeout,
        }),
        _ = cancel => None,
    };
    if decision != Some(Decision::Accepted) {
        PENDING.lock().unwrap().as_mut().map(|m| m.remove(&id));
        emit(&format!("@lunaris-approval-cancel {{\"id\":{}}}", id));
    }
    match decision {
        Some(d) => info!("Connection from {:?}: {}", requester, d.status()),
        None => info!("Connection from {:?}: withdrawn before an answer", requester),
    }
    decision
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        assert_eq!(ApproveMode::parse("password"), ApproveMode::Password);
        assert_eq!(ApproveMode::parse("click"), ApproveMode::Click);
        assert_eq!(ApproveMode::parse("password-click"), ApproveMode::PasswordClick);
        assert_eq!(ApproveMode::parse(""), ApproveMode::PasswordClick);
    }

    // One test: they share the global state.
    #[tokio::test]
    async fn asking() {
        assert_eq!(ask("x", Kind::Control).await, Decision::Unattended);
        ATTACHED.store(true, Ordering::SeqCst);
        let task = tokio::spawn(async { ask("someone", Kind::Files).await });
        // Wait for the question to be registered, then accept it.
        let id = loop {
            if let Some(id) = PENDING.lock().unwrap().as_ref().and_then(|m| m.keys().next().copied()) {
                break id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        answer(id, true);
        assert_eq!(task.await.unwrap(), Decision::Accepted);
        ATTACHED.store(false, Ordering::SeqCst);
    }
}
