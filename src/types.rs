//! Shared domain types. Owned by the integrator — agents must not change
//! existing items here (adding new helper methods is OK if coordinated).

use serde::{Deserialize, Serialize};

/// Supported KiCad major versions. Anything older is unsupported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum KicadVersion {
    #[serde(rename = "9")]
    V9,
    #[serde(rename = "10")]
    V10,
}

impl KicadVersion {
    pub const ALL: [KicadVersion; 2] = [KicadVersion::V9, KicadVersion::V10];

    pub fn major(self) -> u32 {
        match self {
            KicadVersion::V9 => 9,
            KicadVersion::V10 => 10,
        }
    }

    pub fn from_major(m: u32) -> Option<Self> {
        match m {
            9 => Some(KicadVersion::V9),
            10 => Some(KicadVersion::V10),
            _ => None,
        }
    }
}

/// Where a symbol / footprint library comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibSource {
    /// Official KiCad library (matching the file's KiCad version).
    Stock,
    /// Listed in the project's sym-lib-table / fp-lib-table and found inside the repo.
    Project,
    /// Not in any table, but `<nick>.kicad_sym` / `<nick>.pretty` exists inside the project dir.
    ProjectUnregistered,
    /// Not found anywhere (only the copy embedded in the schematic/board exists).
    Unresolved,
}

impl LibSource {
    /// "Local library" in the spec sense: anything that is not stock.
    pub fn is_custom(self) -> bool {
        !matches!(self, LibSource::Stock)
    }
}

/// `Nickname:Item` identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LibId {
    pub nickname: String,
    pub item: String,
}

impl LibId {
    /// Parse `Nick:Item`. Without a colon the nickname is empty.
    pub fn parse(s: &str) -> LibId {
        match s.split_once(':') {
            Some((n, i)) => LibId {
                nickname: n.to_string(),
                item: i.to_string(),
            },
            None => LibId {
                nickname: String::new(),
                item: s.to_string(),
            },
        }
    }
}

impl std::fmt::Display for LibId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.nickname.is_empty() {
            write!(f, "{}", self.item)
        } else {
            write!(f, "{}:{}", self.nickname, self.item)
        }
    }
}
