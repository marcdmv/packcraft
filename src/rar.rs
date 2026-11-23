//! RAR extraction (RAR 2.9/4.x and RAR 5). Placeholder until the reader lands.

use std::path::{Path, PathBuf};

use crate::formats::Item;

pub fn list(_parts: &[PathBuf], _password: Option<&str>) -> Result<Vec<Item>, String> {
    Err("RAR support is not built yet".into())
}

pub fn extract(_parts: &[PathBuf], _dest: Option<&Path>, _password: Option<&str>, _keep: &dyn Fn(&str) -> bool) -> Result<(usize, u64), String> {
    Err("RAR support is not built yet".into())
}
