pub use crate::test_helpers::{fixture_path, AftProcess};

pub fn json_string(value: &impl std::fmt::Display) -> String {
    serde_json::to_string(&value.to_string()).unwrap()
}
