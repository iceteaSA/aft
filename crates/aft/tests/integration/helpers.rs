pub use crate::test_helpers::{cargo_manifest_dir, fixture_path, AftProcess};

pub fn json_string(value: &impl std::fmt::Display) -> String {
    serde_json::to_string(&value.to_string()).unwrap()
}
