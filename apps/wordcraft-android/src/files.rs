//! The framework's file service over the Storage Access Framework bridge.
//!
//! A pick asks Kotlin for the system picker through [`Bridge`]; the picked file's bytes come back
//! as an answer ([`Picked::Contents`], like the browser's, because Android gives no path to reopen).
//! A save hands the bytes to the system's save picker and answers [`Picked::Downloaded`] once it
//! has written them, so the document keeps no file and the next Save asks again. A file Android
//! delivers with no pick outstanding (a share or "open with") goes to the app's intake like a
//! dropped file.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use craft::host::{FileError, Files, PickId, PickKind, PickRequest, Picked, WriteIntent};
use craft::platform::open::{DropTarget, DroppedFile, IntakeSender};

use crate::bridge::{Bridge, SaveResult};

/// The name a save gets when the request names none.
const DEFAULT_NAME: &str = "Document.docx";

#[derive(Default)]
struct State {
    next: u64,
    /// Open pickers shown and not yet answered, oldest first.
    opens: VecDeque<PickId>,
    /// Bridge save id → (ticket, file name).
    saves: HashMap<u64, (PickId, String)>,
    /// Answers waiting for `take_answer`, by ticket.
    answers: HashMap<u64, Picked>,
}

impl State {
    fn ticket(&mut self) -> PickId {
        self.next += 1;
        PickId(self.next)
    }
}

/// The file service of the Android shell.
pub struct AndroidFiles {
    bridge: Arc<Bridge>,
    state: Mutex<State>,
}

impl AndroidFiles {
    pub fn new(bridge: Arc<Bridge>) -> Self {
        Self { bridge, state: Mutex::new(State::default()) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Moves what Kotlin finished into answers and the intake. Returns whether a picker or a save
    /// is still open, so the shell keeps polling.
    pub fn pump(&self, intake: &IntakeSender) -> bool {
        let polled = self.bridge.poll();
        let mut state = self.lock();
        for (name, bytes) in polled.files {
            match state.opens.pop_front() {
                Some(ticket) => {
                    state.answers.insert(ticket.0, Picked::Contents(vec![(name, bytes)]));
                }
                None => {
                    let file = DroppedFile { path: None, name, bytes: Some(Arc::from(bytes)) };
                    intake.dropped(vec![file], DropTarget::Elsewhere);
                }
            }
        }
        for _ in polled.cancelled_opens {
            if let Some(ticket) = state.opens.pop_front() {
                state.answers.insert(ticket.0, Picked::Cancelled);
            }
        }
        for (id, result) in polled.saves {
            if let Some((ticket, name)) = state.saves.remove(&id) {
                let picked = match result {
                    SaveResult::Saved => Picked::Downloaded(name),
                    SaveResult::Cancelled => Picked::Cancelled,
                    SaveResult::Failed(reason) => Picked::Failed(reason),
                };
                state.answers.insert(ticket.0, picked);
            }
        }
        !state.opens.is_empty() || !state.saves.is_empty()
    }
}

fn with_path(e: std::io::Error, path: &Path) -> FileError {
    match FileError::from(e) {
        FileError::NotFound(_) => FileError::NotFound(path.to_owned()),
        FileError::PermissionDenied(_) => FileError::PermissionDenied(path.to_owned()),
        other => other,
    }
}

impl Files for AndroidFiles {
    fn pick(&self, request: PickRequest) -> PickId {
        let mut state = self.lock();
        let ticket = state.ticket();
        match request.kind {
            PickKind::Open | PickKind::OpenMany => match self.bridge.request_open(&request.purpose) {
                Ok(_) => state.opens.push_back(ticket),
                Err(e) => {
                    log::error!("couldn't ask for the open picker: {e}");
                    state.answers.insert(ticket.0, Picked::Unavailable(e.to_string()));
                }
            },
            PickKind::Folder => {
                state.answers.insert(ticket.0, Picked::Unavailable("folders cannot be picked on Android".into()));
            }
            // A save goes through `save`, which carries the bytes.
            PickKind::Save => {
                state.answers.insert(ticket.0, Picked::Unavailable("a save needs its bytes".into()));
            }
        }
        ticket
    }

    fn take_answer(&self, id: PickId) -> Option<Picked> {
        self.lock().answers.remove(&id.0)
    }

    fn forget(&self, id: PickId) {
        let mut state = self.lock();
        state.answers.remove(&id.0);
        state.opens.retain(|t| *t != id);
        state.saves.retain(|_, (t, _)| *t != id);
    }

    fn pick_pending(&self) -> bool {
        let state = self.lock();
        !state.opens.is_empty() || !state.saves.is_empty()
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>, FileError> {
        std::fs::read(path).map_err(|e| with_path(e, path))
    }

    fn write(&self, path: &Path, bytes: &[u8], _intent: WriteIntent) -> Result<bool, FileError> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let tmp = path.with_extension(format!("{ext}.tmp"));
        std::fs::write(&tmp, bytes).map_err(|e| with_path(e, path))?;
        std::fs::rename(&tmp, path).map_err(|e| with_path(e, path))?;
        Ok(false)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn last_folder(&self, _purpose: &str) -> Option<PathBuf> {
        None
    }

    fn save(&self, request: PickRequest, bytes: Vec<u8>) -> PickId {
        let mut state = self.lock();
        let ticket = state.ticket();
        let name = request.file_name.clone().unwrap_or_else(|| DEFAULT_NAME.to_string());
        match self.bridge.request_save(&name, &bytes) {
            Ok(id) => {
                state.saves.insert(id, (ticket, name));
            }
            Err(e) => {
                state.answers.insert(ticket.0, Picked::Failed(format!("couldn't hand the file to Android: {e}")));
            }
        }
        ticket
    }

    /// A pick answers bytes, not a path that `read` opens again.
    fn can_pick(&self) -> bool {
        false
    }

    /// The bytes go to a system picker, not to a path.
    fn browser_saves(&self) -> bool {
        true
    }
}
