//! Default profiles embedded in the binary and copied into a new collective.

pub struct DefaultProfile {
    pub name: &'static str,
    pub toml: &'static str,
    pub prompt: &'static str,
}

macro_rules! profile {
    ($name:literal) => {
        DefaultProfile {
            name: $name,
            toml: include_str!(concat!("../../../profiles/", $name, ".toml")),
            prompt: include_str!(concat!("../../../profiles/", $name, ".md")),
        }
    };
}

pub const DEFAULT_PROFILES: &[DefaultProfile] = &[
    profile!("supervisor"),
    profile!("surveyor"),
    profile!("planner"),
    profile!("coder"),
    profile!("reviewer"),
    profile!("tester"),
    profile!("deployer"),
];

pub const GITIGNORE: &str = "# vflt: machine-local liveness state and per-item workspaces are not part of the board\nwork/\nagents/*.hb\nitems/*/.claim\n";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;

    #[test]
    fn shipped_profiles_parse() {
        for d in DEFAULT_PROFILES {
            let p = Profile::parse(d.toml, d.prompt)
                .unwrap_or_else(|e| panic!("profile {} failed to parse: {e}", d.name));
            assert_eq!(p.name, d.name);
            assert!(
                !p.prompt.trim().is_empty(),
                "{} has an empty prompt",
                d.name
            );
        }
    }

    #[test]
    fn supervisor_is_cycle_only_and_others_have_stages() {
        for d in DEFAULT_PROFILES {
            let p = Profile::parse(d.toml, d.prompt).unwrap();
            if d.name == "supervisor" {
                assert!(p.is_cycle_only());
            } else {
                assert!(!p.stages.is_empty(), "{} should handle a stage", d.name);
            }
        }
    }
}
