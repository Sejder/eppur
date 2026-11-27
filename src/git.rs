use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct Git {
    dir: PathBuf,
}

impl Git {
    /// The work tree at `dir`, if it is one. When git refuses a repository it
    /// can see (another user's, without safe.directory), that is said, not
    /// skipped: without git, pins.json would not reach the flake.
    pub fn open(dir: &Path) -> Option<Self> {
        let git = Self {
            dir: dir.to_path_buf(),
        };
        let out = match git
            .command()
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
        {
            Ok(out) => out,
            Err(e) => {
                if dir.ancestors().any(|d| d.join(".git").exists()) {
                    crate::log::warn(&format!(
                        "{} is a git repository, but git cannot be run ({e}); nothing will be committed, and a new pins.json will not reach the flake",
                        dir.display()
                    ));
                }
                return None;
            }
        };
        if out.status.success() {
            return Some(git);
        }
        if dir.ancestors().any(|d| d.join(".git").exists()) {
            crate::log::warn(&format!(
                "git refuses {}: {}",
                dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        None
    }

    fn command(&self) -> Command {
        let mut c = Command::new("git");
        c.arg("-C").arg(&self.dir).stdin(Stdio::null());
        c
    }

    pub fn tracked(&self, path: &Path) -> bool {
        self.command()
            .args(["ls-files", "--error-unmatch", "--"])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Flakes ignore untracked files; --intent-to-add makes the file and its
    /// later edits visible without staging any content.
    pub fn intent_to_add(&self, path: &Path) -> Result<()> {
        let status = self
            .command()
            .args(["add", "--intent-to-add", "--"])
            .arg(path)
            .status()
            .context("spawning git add")?;
        if !status.success() {
            bail!("git add --intent-to-add {} failed", path.display());
        }
        Ok(())
    }

    fn rev_parse(&self, args: &[&str]) -> Option<String> {
        let out = self.command().arg("rev-parse").args(args).output().ok()?;
        out.status.success().then(|| {
            String::from_utf8_lossy(&out.stdout)
                .trim_end_matches('\n')
                .to_string()
        })
    }

    /// The repository's git directory, also for worktrees, where .git is a file.
    pub fn git_dir(&self) -> Option<PathBuf> {
        self.rev_parse(&["--absolute-git-dir"]).map(PathBuf::from)
    }

    /// Where the flake sits below the repository's top, e.g. "nixos/" for a
    /// flake in a subdirectory; empty at the top.
    pub fn prefix(&self) -> String {
        self.rev_parse(&["--show-prefix"]).unwrap_or_default()
    }

    /// The abbreviated hash of HEAD, for reporting a commit.
    pub fn head(&self) -> Option<String> {
        self.rev_parse(&["--short", "HEAD"])
    }

    /// Undo --intent-to-add for a file this run created.
    pub fn forget(&self, path: &Path) -> Result<()> {
        let status = self
            .command()
            .args(["rm", "--cached", "--quiet", "--ignore-unmatch", "--"])
            .arg(path)
            .status()
            .context("spawning git rm")?;
        if !status.success() {
            bail!("git rm --cached {} failed", path.display());
        }
        Ok(())
    }

    pub fn changed(&self, paths: &[&Path]) -> Result<bool> {
        let out = self
            .command()
            .args(["status", "--porcelain", "--untracked-files=no", "--"])
            .args(paths)
            .output()
            .context("spawning git status")?;
        if !out.status.success() {
            bail!(
                "git status failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(!out.stdout.is_empty())
    }

    /// Commit exactly `paths`, leaving everything else in the tree alone.
    /// Returns false when there was nothing to commit.
    pub fn commit(&self, paths: &[&Path], message: &str) -> Result<bool> {
        if !self.changed(paths)? {
            return Ok(false);
        }
        let out = self
            .command()
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-q",
                "--no-verify",
                "-m",
                message,
                "--",
            ])
            .args(paths)
            .output()
            .context("spawning git commit")?;
        if !out.status.success() {
            bail!(
                "git commit failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(true)
    }
}
