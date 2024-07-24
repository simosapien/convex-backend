use common::components::ComponentPath;
use isolate::UdfArgsJson;
use keybroker::Identity;
use serde::Deserialize;

use crate::admin::bad_admin_key_error;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
/// This struct should only be used for endpoints that allow calling functions
/// inside components. Requires admin key.
pub struct UdfPostRequestWithComponent {
    component_path: Option<String>,
    pub path: String,
    pub args: UdfArgsJson,

    pub format: Option<String>,
}

impl UdfPostRequestWithComponent {
    pub fn component_path(&self, identity: &Identity) -> anyhow::Result<ComponentPath> {
        anyhow::ensure!(identity.is_admin(), bad_admin_key_error(None));
        ComponentPath::deserialize(self.component_path.as_deref())
    }
}