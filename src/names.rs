use rand::seq::IndexedRandom;
use std::{fmt, path::Path};

/// A single workspace directory name, never a path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct WorkspaceName(String);

impl WorkspaceName {
    pub fn parse(name: &str) -> anyhow::Result<Self> {
        name.parse()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for WorkspaceName {
    type Error = anyhow::Error;

    fn try_from(name: String) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !name.is_empty()
                && !matches!(name.as_str(), "." | "..")
                && !name.contains(['/', '\\', '\0'])
                && matches!(
                    Path::new(&name).components().next(),
                    Some(std::path::Component::Normal(_))
                ),
            "invalid workspace name {name:?}"
        );
        Ok(Self(name))
    }
}

impl std::str::FromStr for WorkspaceName {
    type Err = anyhow::Error;

    fn from_str(name: &str) -> anyhow::Result<Self> {
        Self::try_from(name.to_owned())
    }
}

impl AsRef<Path> for WorkspaceName {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl fmt::Display for WorkspaceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

const ADJECTIVES: &[&str] = &[
    "amber", "bold", "calm", "dark", "eager", "fair", "glad", "hazy", "icy", "jade", "keen",
    "lush", "mild", "neat", "opal", "pale", "quick", "rosy", "soft", "tidy", "vast", "warm",
    "zany", "aqua", "blue", "crisp", "dusty", "ember", "fresh", "gold", "happy", "ivory", "jolly",
    "kind", "lazy", "merry", "noble", "olive", "plum", "quiet", "rapid", "sage", "tall", "ultra",
    "vivid", "wise", "young", "zen", "agile", "brave",
];

const NOUNS: &[&str] = &[
    "ant", "bat", "cat", "dog", "elk", "fox", "gnu", "hawk", "ibis", "jay", "koi", "lynx", "mole",
    "newt", "owl", "puma", "quail", "ram", "seal", "toad", "vole", "wolf", "yak", "crab", "dart",
    "eel", "frog", "goat", "hare", "inca", "koala", "lamb", "mink", "narwhal", "orca", "panda",
    "raven", "swan", "tiger", "urchin", "viper", "wren", "zebra", "bear", "crow", "dove", "egret",
    "finch", "gull", "heron",
];

/// Generate a random `adjective-noun` workspace name.
pub fn generate_name() -> String {
    let mut rng = rand::rng();
    let adj = ADJECTIVES.choose(&mut rng).unwrap();
    let noun = NOUNS.choose(&mut rng).unwrap();
    format!("{adj}-{noun}")
}

/// Generate a random `adjective-noun` name that does not already exist as a
/// subdirectory of `dir`.
pub fn generate_unique(dir: &Path) -> String {
    loop {
        let name = generate_name();
        if !dir.join(&name).exists() {
            return name;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_names_are_single_components() {
        for raw in [".hidden", "with spaces", "feature-1"] {
            assert_eq!(WorkspaceName::parse(raw).unwrap().as_str(), raw);
        }
        for raw in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
            assert!(WorkspaceName::parse(raw).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn name_has_adjective_dash_noun_format() {
        let name = generate_name();
        let parts: Vec<&str> = name.splitn(2, '-').collect();
        assert_eq!(parts.len(), 2);
        assert!(ADJECTIVES.contains(&parts[0]));
        assert!(NOUNS.contains(&parts[1]));
    }

    #[test]
    fn generate_unique_avoids_collisions() {
        let dir = tempfile::tempdir().unwrap();
        // Create a bunch of names and ensure they all get unique ones
        let mut names = std::collections::HashSet::new();
        for _ in 0..20 {
            let name = generate_unique(dir.path());
            // Create a directory with that name so it becomes "taken"
            std::fs::create_dir(dir.path().join(&name)).unwrap();
            assert!(names.insert(name));
        }
    }
}
