use rand::seq::IndexedRandom;
use std::path::Path;

const ADJECTIVES: &[&str] = &[
    "amber", "bold", "calm", "dark", "eager", "fair", "glad", "hazy", "icy", "jade",
    "keen", "lush", "mild", "neat", "opal", "pale", "quick", "rosy", "soft", "tidy",
    "vast", "warm", "zany", "aqua", "blue", "crisp", "dusty", "ember", "fresh", "gold",
    "happy", "ivory", "jolly", "kind", "lazy", "merry", "noble", "olive", "plum", "quiet",
    "rapid", "sage", "tall", "ultra", "vivid", "wise", "young", "zen", "agile", "brave",
];

const NOUNS: &[&str] = &[
    "ant", "bat", "cat", "dog", "elk", "fox", "gnu", "hawk", "ibis", "jay",
    "koi", "lynx", "mole", "newt", "owl", "puma", "quail", "ram", "seal", "toad",
    "vole", "wolf", "yak", "crab", "dart", "eel", "frog", "goat", "hare", "inca",
    "koala", "lamb", "mink", "narwhal", "orca", "panda", "raven", "swan", "tiger", "urchin",
    "viper", "wren", "zebra", "bear", "crow", "dove", "egret", "finch", "gull", "heron",
];

pub fn generate_name() -> String {
    let mut rng = rand::rng();
    let adj = ADJECTIVES.choose(&mut rng).unwrap();
    let noun = NOUNS.choose(&mut rng).unwrap();
    format!("{adj}-{noun}")
}

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
