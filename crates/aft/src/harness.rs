use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Harness {
    Opencode,
    Pi,
    Runner,
    Mcp { client: String },
}

impl Harness {
    pub fn storage_segment(&self) -> String {
        match self {
            Harness::Opencode => "opencode".to_string(),
            Harness::Pi => "pi".to_string(),
            Harness::Runner => "runner".to_string(),
            Harness::Mcp { client } => format!("mcp--{}", sanitize_client(client)),
        }
    }

    pub fn wire_label(&self) -> String {
        match self {
            Harness::Opencode => "opencode".to_string(),
            Harness::Pi => "pi".to_string(),
            Harness::Runner => "runner".to_string(),
            Harness::Mcp { client } => format!("mcp:{client}"),
        }
    }
}

fn sanitize_client(client: &str) -> String {
    let lower = client.to_ascii_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut last_was_dash = false;
    for ch in lower.chars() {
        let keep = ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-');
        if keep {
            out.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    let trimmed = out.trim_matches(|c| c == '-' || c == '.');
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

impl Serialize for Harness {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.wire_label())
    }
}

impl<'de> Deserialize<'de> for Harness {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct HarnessVisitor;

        impl<'de> Visitor<'de> for HarnessVisitor {
            type Value = Harness;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .write_str("a harness string: 'opencode', 'pi', 'runner', or 'mcp:<client>'")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Harness::from_str(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(HarnessVisitor)
    }
}

impl fmt::Display for Harness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.wire_label())
    }
}

impl std::str::FromStr for Harness {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "opencode" => Ok(Harness::Opencode),
            "pi" => Ok(Harness::Pi),
            "runner" => Ok(Harness::Runner),
            other if other.starts_with("mcp:") => {
                let client = &other[4..];
                if client.is_empty() {
                    Err(
                        "unsupported harness 'mcp:'; mcp client name must be non-empty".to_string(),
                    )
                } else {
                    Ok(Harness::Mcp {
                        client: client.to_string(),
                    })
                }
            }
            other => Err(format!(
                "unsupported harness '{other}'; expected 'opencode', 'pi', 'runner', or 'mcp:<client>'"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{sanitize_client, Harness};
    use std::str::FromStr;

    #[test]
    fn harness_enum_serde_roundtrip() {
        assert_eq!(
            serde_json::to_string(&Harness::Opencode).unwrap(),
            "\"opencode\""
        );
        assert_eq!(serde_json::to_string(&Harness::Pi).unwrap(), "\"pi\"");

        assert_eq!(
            serde_json::from_str::<Harness>("\"opencode\"").unwrap(),
            Harness::Opencode
        );
        assert_eq!(
            serde_json::from_str::<Harness>("\"pi\"").unwrap(),
            Harness::Pi
        );
        assert!(serde_json::from_str::<Harness>("\"claude_code\"").is_err());
    }

    #[test]
    fn opencode_pi_storage_segment_unchanged() {
        assert_eq!(Harness::Opencode.storage_segment(), "opencode");
        assert_eq!(Harness::Pi.storage_segment(), "pi");
    }

    #[test]
    fn runner_round_trips() {
        assert_eq!(Harness::from_str("runner").unwrap(), Harness::Runner);
        assert_eq!(Harness::Runner.storage_segment(), "runner");
        assert_eq!(
            serde_json::to_string(&Harness::Runner).unwrap(),
            "\"runner\""
        );
        assert_eq!(
            serde_json::from_str::<Harness>("\"runner\"").unwrap(),
            Harness::Runner
        );
    }

    #[test]
    fn mcp_round_trips() {
        let h = Harness::Mcp {
            client: "claude-code".to_string(),
        };
        assert_eq!(serde_json::to_string(&h).unwrap(), "\"mcp:claude-code\"");
        assert_eq!(
            serde_json::from_str::<Harness>("\"mcp:claude-code\"").unwrap(),
            h
        );
        assert_eq!(
            Harness::from_str("mcp:cursor").unwrap(),
            Harness::Mcp {
                client: "cursor".to_string(),
            }
        );
        assert!(Harness::from_str("mcp:").is_err());
    }

    #[test]
    fn storage_segment_hostile_clients_are_path_safe() {
        let cases = ["../../etc", "a/b", r"a\b", "a:b", "", "Claude.Code"];
        for client in cases {
            let seg = Harness::Mcp {
                client: client.to_string(),
            }
            .storage_segment();
            assert!(
                !seg.is_empty(),
                "segment must be non-empty for client {client:?}"
            );
            assert!(
                !seg.contains(['/', '\\', ':']),
                "segment {seg:?} must not contain path separators for client {client:?}"
            );
            assert!(
                !seg.contains(".."),
                "segment {seg:?} must not contain '..' for client {client:?}"
            );
            assert!(
                seg.starts_with("mcp--"),
                "segment {seg:?} must use mcp-- prefix"
            );
        }
        assert_eq!(
            Harness::Mcp {
                client: "Claude.Code".to_string(),
            }
            .storage_segment(),
            "mcp--claude.code"
        );
        assert_eq!(sanitize_client(""), "unknown");
    }
}
