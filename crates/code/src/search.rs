use anyhow::Result;

use crate::{SearchInput, SearchOutput, Workspace};

pub(crate) async fn execute(workspace: &Workspace, input: &SearchInput) -> Result<SearchOutput> {
    workspace.search(input).await
}
