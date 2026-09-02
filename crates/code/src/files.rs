use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{FILE_LIMIT, FileList, Workspace, WorkspaceToolOutput};

type FilesToolOutput = WorkspaceToolOutput<FileList>;

/// List at most 100 visible workspace files, optionally under one relative path and matching one non-negated glob.
#[lutum::tool_input(name = "files", output = FilesToolOutput)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub(crate) struct FilesInput {
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

pub(crate) async fn execute(workspace: &Workspace, input: &FilesInput) -> Result<FileList> {
    workspace
        .visible_files(input.glob.as_deref(), input.path.as_deref(), FILE_LIMIT)
        .await
}
