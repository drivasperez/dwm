use rand::seq::IndexedRandom;
use std::path::Path;

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

/// Random picks to try before searching the space in order.
const RANDOM_ATTEMPTS: usize = 64;

/// Generate a random `adjective-noun` name that does not already exist as a
/// subdirectory of `dir`.
///
/// Always terminates: after a few random picks it walks every combination,
/// and once all of them are taken it adds a numeric suffix.
pub fn generate_unique(dir: &Path) -> String {
    let free = |name: &str| !dir.join(name).exists();
    for _ in 0..RANDOM_ATTEMPTS {
        let name = generate_name();
        if free(&name) {
            return name;
        }
    }
    for adj in ADJECTIVES {
        for noun in NOUNS {
            let name = format!("{adj}-{noun}");
            if free(&name) {
                return name;
            }
        }
    }
    let base = generate_name();
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|name| free(name))
        .expect("an unbounded range of suffixes always has a free one")
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

    #[test]
    fn generate_unique_terminates_when_every_combination_is_taken() {
        // Regression: this looped forever once all 2,500 names existed.
        let tmp = tempfile::tempdir().unwrap();
        for adj in ADJECTIVES {
            for noun in NOUNS {
                std::fs::create_dir(tmp.path().join(format!("{adj}-{noun}"))).unwrap();
            }
        }
        let name = generate_unique(tmp.path());
        assert!(!tmp.path().join(&name).exists(), "{name} is taken");
        let (base, suffix) = name.rsplit_once('-').unwrap();
        assert!(suffix.parse::<u32>().unwrap() >= 2, "{name}");
        assert!(tmp.path().join(base).exists(), "suffixed onto a real name");
    }

    #[test]
    fn generate_unique_finds_the_last_free_combination() {
        let tmp = tempfile::tempdir().unwrap();
        let last_free = format!("{}-{}", ADJECTIVES[7], NOUNS[13]);
        for adj in ADJECTIVES {
            for noun in NOUNS {
                let name = format!("{adj}-{noun}");
                if name != last_free {
                    std::fs::create_dir(tmp.path().join(name)).unwrap();
                }
            }
        }
        assert_eq!(generate_unique(tmp.path()), last_free);
    }
}
