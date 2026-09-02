use anyhow::Result;

use crate::{ReadInput, ReadOutput, Workspace};

pub(crate) async fn execute(workspace: &Workspace, input: &ReadInput) -> Result<ReadOutput> {
    workspace.read(input).await
}
