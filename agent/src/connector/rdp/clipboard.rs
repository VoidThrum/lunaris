//! Clipboard text over the CLIPRDR static channel (MS-RDPECLIP), through
//! ironrdp-cliprdr. Text only: CF_UNICODETEXT (MS-RDPECLIP 2.2.5.1, the
//! UTF-16 text format, as ironrdp-web `clipboard.rs` uses it).
//!
//! ironrdp-cliprdr calls this backend from inside the channel; what needs an
//! answer from the session (ask for the remote text, give ours) is queued
//! here and done by the session's loop, as ironrdp-client does with its
//! `ClipboardMessage` proxy (rdp.rs `RdpInputEvent::Clipboard`).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ironrdp_cliprdr::backend::CliprdrBackend;
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest, FileContentsResponse, FormatDataRequest,
    FormatDataResponse, LockDataId,
};
use ironrdp_core::impl_as_any;

/// What the session's loop has to do for the channel.
#[derive(Debug, PartialEq)]
pub enum ClipboardTask {
    /// The channel is up: tell the server our formats if there is text.
    Ready,
    /// The server asks for our format list (MS-RDPECLIP 2.2.3.1).
    SendFormats,
    /// The other computer copied text: ask for it.
    FetchRemoteText,
    /// The text came.
    RemoteText(String),
    /// The server asks for our text.
    SendLocalText,
    /// Our paste request failed.
    RemoteTextFailed,
}

pub type Tasks = Arc<Mutex<VecDeque<ClipboardTask>>>;

#[derive(Debug)]
pub struct TextBackend {
    tasks: Tasks,
}

impl_as_any!(TextBackend);

impl TextBackend {
    pub fn new(tasks: Tasks) -> TextBackend {
        TextBackend { tasks }
    }

    fn push(&self, task: ClipboardTask) {
        self.tasks.lock().unwrap().push_back(task);
    }
}

pub fn unicode_text_format() -> ClipboardFormat {
    ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)
}

impl CliprdrBackend for TextBackend {
    fn temporary_directory(&self) -> &str {
        // No files are exchanged; ironrdp-web names its own folder the same way.
        ".cliprdr"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        // Text only: none of the file-transfer capabilities.
        ClipboardGeneralCapabilityFlags::empty()
    }

    fn on_ready(&mut self) {
        self.push(ClipboardTask::Ready);
    }

    fn on_request_format_list(&mut self) {
        self.push(ClipboardTask::SendFormats);
    }

    fn on_process_negotiated_capabilities(&mut self, _capabilities: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        if available_formats.iter().any(|f| f.id() == ClipboardFormatId::CF_UNICODETEXT) {
            self.push(ClipboardTask::FetchRemoteText);
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        if request.format == ClipboardFormatId::CF_UNICODETEXT {
            self.push(ClipboardTask::SendLocalText);
        }
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if response.is_error() {
            self.push(ClipboardTask::RemoteTextFailed);
            return;
        }
        match response.to_unicode_string() {
            Ok(text) => self.push(ClipboardTask::RemoteText(text)),
            Err(_) => self.push(ClipboardTask::RemoteTextFailed),
        }
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}
