mod faculty;
mod files;
mod patch;
mod read;
mod search;
#[cfg(test)]
mod test_support;
mod workspace;

pub use faculty::{CodeModule, FacultyBatch, registrar, session_auto_compaction};
pub use patch::{PatchDecision, PatchGate, PatchUiEvent, PendingPatch};
pub use workspace::{
    FILE_LIMIT, FileList, READ_BYTE_LIMIT, READ_LINE_LIMIT, ReadInput, ReadOutput, RgExecutable,
    RgOutput, SEARCH_FRAGMENT_BYTE_LIMIT, SEARCH_MATCH_LIMIT, SearchInput, SearchMatch,
    SearchOutput, Workspace, WorkspaceToolOutput, sha256_hex,
};

pub(crate) use workspace::truncate_utf8;
