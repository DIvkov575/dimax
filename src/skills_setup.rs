//! Installs the bundled `dimax-pane-control` Claude Code skill into the
//! user's global skills directory (`~/.claude/skills/`), so any project's
//! Claude session can drive `dimax server ...` without a local copy. The
//! skill's content ships inside the `dimax` binary itself (`include_str!`)
//! so this works from a `cargo install`/Homebrew install, not just a
//! checkout of this repo.

const SKILL_MD: &str = include_str!("../.claude/skills/dimax-pane-control/SKILL.md");
const SKILL_NAME: &str = "dimax-pane-control";

/// `~/.claude/skills/<SKILL_NAME>/SKILL.md` -- the fixed, well-known path
/// Claude Code reads global skills from. Not XDG-configurable (unlike
/// dimax's own config), so this always resolves under `$HOME`.
fn skill_path() -> anyhow::Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow::anyhow!("cannot locate ~/.claude/skills: HOME is unset"))?;
    Ok(std::path::PathBuf::from(home)
        .join(".claude")
        .join("skills")
        .join(SKILL_NAME)
        .join("SKILL.md"))
}

/// Write the bundled skill to `~/.claude/skills/dimax-pane-control/SKILL.md`,
/// overwriting any previous copy -- this is always a straight copy of what
/// shipped with the running binary, so an existing file is always safe to
/// replace (unlike `kitty_setup::install`, there is no user-owned variant
/// of this file to preserve).
pub fn install() -> anyhow::Result<std::path::PathBuf> {
    let path = skill_path()?;
    let parent = path
        .parent()
        .expect("skill path always has a parent (joined above)");
    std::fs::create_dir_all(parent)?;
    std::fs::write(&path, SKILL_MD)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate `HOME` (process-global) so two can't
    /// stomp on each other's env state mid-test -- same pattern as
    /// `kitty_setup`'s `ENV_LOCK`.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_fake_home<T>(home: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", home);
        }
        let result = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        result
    }

    #[test]
    fn install_writes_the_bundled_skill_under_dot_claude_skills() {
        let dir = std::env::temp_dir().join(format!("dmx-skills-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = with_fake_home(&dir, || install().unwrap());
        assert_eq!(
            path,
            dir.join(".claude")
                .join("skills")
                .join(SKILL_NAME)
                .join("SKILL.md")
        );
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, SKILL_MD);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_overwrites_a_previous_copy() {
        let dir = std::env::temp_dir().join(format!("dmx-skills-test-{}", std::process::id() + 1));
        std::fs::create_dir_all(&dir).unwrap();
        with_fake_home(&dir, || {
            let path = install().unwrap();
            std::fs::write(&path, "stale content").unwrap();
            install().unwrap();
            let written = std::fs::read_to_string(&path).unwrap();
            assert_eq!(written, SKILL_MD);
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
