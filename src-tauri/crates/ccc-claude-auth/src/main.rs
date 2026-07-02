use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};

#[derive(Parser)]
#[command(
    name = "ccc-claude-auth",
    version,
    about = "Claude Code Keychain helper for ccc"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Hash {
        #[arg(long)]
        config_dir: PathBuf,
    },
    GetCredentials {
        #[arg(long)]
        config_dir: PathBuf,
    },
}

fn keychain_account_for(config_dir: &Path) -> String {
    let s = config_dir.to_string_lossy();
    let trimmed = s.trim_end_matches('/');
    let mut hasher = Sha256::new();
    hasher.update(trimmed.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(8);
    const HEX: &[u8] = b"0123456789abcdef";
    for b in &digest[..4] {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn fetch_credentials(config_dir: &Path) -> Result<String> {
    let hash8 = keychain_account_for(config_dir);
    let service = format!("Claude Code-credentials-{hash8}");
    let user = std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .context("USER environment variable not set")?;

    let output = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", &service, "-a", &user, "-w"])
        .output()
        .context("failed to spawn /usr/bin/security")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "security command failed (service={service}, account={user}): {}",
            stderr.trim()
        );
    }

    let raw = String::from_utf8(output.stdout).context("security stdout was not UTF-8")?;
    Ok(raw.trim_end_matches('\n').to_string())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Hash { config_dir } => {
            println!("{}", keychain_account_for(&config_dir));
        }
        Cmd::GetCredentials { config_dir } => {
            let creds = fetch_credentials(&config_dir)?;
            println!("{creds}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_known_paths() {
        assert_eq!(
            keychain_account_for(Path::new(
                "/Users/user/.ccc/instances/ccc-example1/claude_config"
            )),
            "df00639b"
        );
        assert_eq!(
            keychain_account_for(Path::new(
                "/Users/user/.ccc/instances/ccc-example2/claude_config"
            )),
            "739bbf4c"
        );
    }

    #[test]
    fn trailing_slash_is_normalized() {
        let a = keychain_account_for(Path::new("/tmp/foo"));
        let b = keychain_account_for(Path::new("/tmp/foo/"));
        assert_eq!(a, b);
    }
}
