//! Who this process is, and who owns a path: for permission errors, which in
//! a container are rarely between the users the operator assumes.

use std::path::Path;

/// This process's uid and groups, from `/proc/self/status`.
pub fn process() -> Option<String> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .map(|v| v.split_whitespace().collect::<Vec<_>>())
    };
    let uid = field("Uid:")?.first()?.to_string();
    let gid = field("Gid:")?.first()?.to_string();
    let groups = field("Groups:").unwrap_or_default().join(",");
    Some(format!("uid {uid} (gid {gid}, groups {groups})"))
}

/// `path`'s owner and mode, e.g. `uid 1000, gid 1000, mode 644`.
#[cfg(unix)]
pub fn owner(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some(format!(
        "uid {}, gid {}, mode {:o}",
        meta.uid(),
        meta.gid(),
        meta.mode() & 0o7777
    ))
}

#[cfg(not(unix))]
pub fn owner(_path: &Path) -> Option<String> {
    None
}

/// Why the mint may not be able to write `file` in `dir`: who owns each,
/// and who this process is. Empty when nothing can be said.
pub fn write_hint(dir: &Path, file: &Path) -> String {
    let mut parts = vec![];
    if let Some(owner) = owner(dir) {
        parts.push(format!("{} is {owner}", dir.display()));
    }
    if let Some(owner) = owner(file) {
        parts.push(format!("{} is {owner}", file.display()));
    }
    if let Some(me) = process() {
        parts.push(format!("this process is {me}"));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!(
        " ({}). The data directory and everything in it must be writable by the user the mint runs \
         as - with Docker, the --user it runs with",
        parts.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_the_process_and_paths() {
        assert!(process().unwrap().starts_with("uid "));
        let dir = std::env::temp_dir();
        let hint = write_hint(&dir, &dir.join("does-not-exist"));
        assert!(hint.contains("this process is uid "), "{hint}");
        assert!(
            hint.contains(&format!("{} is uid ", dir.display())),
            "{hint}"
        );
    }
}
