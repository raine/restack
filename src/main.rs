use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Parser)]
#[command(name = "gh-restack")]
#[command(about = "Rebase stacked PRs onto their current base branches")]
struct Cli {
    /// PR numbers to restack
    #[arg(required = true)]
    prs: Vec<u32>,

    /// Show what would be done without executing
    #[arg(long)]
    dry_run: bool,

    /// Push branches after rebasing (force-with-lease)
    #[arg(long)]
    push: bool,
}

#[derive(Deserialize, Debug)]
struct PrInfo {
    number: u32,
    #[serde(rename = "headRefName")]
    head_ref: String,
    #[serde(rename = "baseRefName")]
    base_ref: String,
}

fn run_cmd(cmd: &mut Command) -> Result<String> {
    let output = cmd.output().context("failed to run command")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let program = cmd.get_program().to_string_lossy().to_string();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        bail!("{} {} failed:\n{}", program, args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_cmd_in(dir: &Path, cmd: &mut Command) -> Result<String> {
    cmd.current_dir(dir);
    run_cmd(cmd)
}

fn get_pr_info(pr_number: u32) -> Result<PrInfo> {
    let output = run_cmd(Command::new("gh").args([
        "pr",
        "view",
        &pr_number.to_string(),
        "--json",
        "number,headRefName,baseRefName",
    ]))
    .with_context(|| format!("failed to get info for PR #{pr_number}"))?;
    serde_json::from_str(&output).with_context(|| format!("failed to parse PR #{pr_number} info"))
}

fn parse_worktree_map(output: &str) -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();
    let mut current_path: Option<PathBuf> = None;

    for line in output.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(path));
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            if let Some(path) = current_path.take() {
                map.insert(branch.to_string(), path);
            }
        } else if line.is_empty() {
            current_path = None;
        }
    }

    map
}

fn get_worktree_map() -> Result<HashMap<String, PathBuf>> {
    let output = run_cmd(Command::new("git").args(["worktree", "list", "--porcelain"]))?;
    Ok(parse_worktree_map(&output))
}

/// Sort PRs by dependency order (topological sort).
/// If PR_B's base_ref matches PR_A's head_ref, PR_A comes first.
fn sort_by_dependency(prs: Vec<PrInfo>) -> Vec<PrInfo> {
    let mut remaining = prs;
    let mut sorted = Vec::with_capacity(remaining.len());

    while !remaining.is_empty() {
        let remaining_heads: std::collections::HashSet<&str> =
            remaining.iter().map(|p| p.head_ref.as_str()).collect();

        let pos = remaining
            .iter()
            .position(|pr| !remaining_heads.contains(pr.base_ref.as_str()));

        match pos {
            Some(idx) => sorted.push(remaining.remove(idx)),
            None => {
                // Cycle or all remaining depend on each other — append in original order
                sorted.extend(remaining);
                break;
            }
        }
    }

    sorted
}

fn check_worktree_clean(dir: &Path) -> Result<()> {
    let status = run_cmd_in(dir, Command::new("git").args(["status", "--porcelain"]))?;
    if !status.is_empty() {
        bail!(
            "working tree is not clean in {}:\n{}",
            dir.display(),
            status
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut prs = Vec::new();
    for &pr_number in &cli.prs {
        let info = get_pr_info(pr_number)?;
        println!("PR #{}: {} → {}", info.number, info.head_ref, info.base_ref);
        prs.push(info);
    }

    let prs = sort_by_dependency(prs);
    let worktree_map = get_worktree_map()?;
    let mut rebased_heads: HashMap<String, PathBuf> = HashMap::new();

    if !cli.dry_run {
        println!("\nFetching origin...");
        run_cmd(Command::new("git").args(["fetch", "origin"]))?;
    }

    println!();

    for pr in &prs {
        let worktree_path = worktree_map.get(&pr.head_ref).ok_or_else(|| {
            anyhow::anyhow!(
                "branch '{}' (PR #{}) is not checked out in any worktree",
                pr.head_ref,
                pr.number
            )
        })?;

        // Rebase onto local branch if it was just rebased, otherwise onto origin/<base>
        let onto = if rebased_heads.contains_key(&pr.base_ref) {
            pr.base_ref.clone()
        } else {
            format!("origin/{}", pr.base_ref)
        };

        if cli.dry_run {
            println!(
                "PR #{}: would rebase '{}' onto '{}' (in {})",
                pr.number,
                pr.head_ref,
                onto,
                worktree_path.display()
            );
            if cli.push {
                println!(
                    "PR #{}: would push '{}' (force-with-lease)",
                    pr.number, pr.head_ref
                );
            }
        } else {
            println!(
                "PR #{}: rebasing '{}' onto '{}'",
                pr.number, pr.head_ref, onto,
            );

            check_worktree_clean(worktree_path)?;

            run_cmd_in(
                worktree_path,
                Command::new("git").args(["rebase", &onto]),
            )
            .with_context(|| {
                format!(
                    "rebase failed for PR #{} — resolve conflicts in {} then run: git rebase --continue",
                    pr.number,
                    worktree_path.display()
                )
            })?;

            if cli.push {
                println!("PR #{}: pushing '{}'", pr.number, pr.head_ref);
                run_cmd_in(
                    worktree_path,
                    Command::new("git").args(["push", "--force-with-lease"]),
                )
                .with_context(|| format!("push failed for PR #{}", pr.number))?;
            }

            println!("PR #{}: done\n", pr.number);
        }

        rebased_heads.insert(pr.head_ref.clone(), worktree_path.clone());
    }

    if cli.dry_run {
        println!("\n(dry run — no changes made)");
    } else {
        println!("All PRs restacked successfully.");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u32, head: &str, base: &str) -> PrInfo {
        PrInfo {
            number,
            head_ref: head.to_string(),
            base_ref: base.to_string(),
        }
    }

    #[test]
    fn sort_independent_prs_preserves_order() {
        let prs = vec![pr(1, "feat-a", "main"), pr(2, "feat-b", "main")];
        let sorted = sort_by_dependency(prs);
        assert_eq!(sorted[0].number, 1);
        assert_eq!(sorted[1].number, 2);
    }

    #[test]
    fn sort_stacked_prs_orders_by_dependency() {
        let prs = vec![
            pr(3, "feat-c", "feat-b"),
            pr(1, "feat-a", "main"),
            pr(2, "feat-b", "feat-a"),
        ];
        let sorted = sort_by_dependency(prs);
        assert_eq!(sorted[0].number, 1);
        assert_eq!(sorted[1].number, 2);
        assert_eq!(sorted[2].number, 3);
    }

    #[test]
    fn sort_partial_stack() {
        let prs = vec![pr(3, "feat-c", "feat-b"), pr(2, "feat-b", "main")];
        let sorted = sort_by_dependency(prs);
        assert_eq!(sorted[0].number, 2);
        assert_eq!(sorted[1].number, 3);
    }

    #[test]
    fn parse_worktree_output() {
        let output = "\
worktree /Users/raine/code/myrepo
branch refs/heads/main

worktree /Users/raine/code/myrepo__worktrees/feat-a
branch refs/heads/feat-a

worktree /Users/raine/code/myrepo__worktrees/feat-b
branch refs/heads/feature/feat-b
";
        let map = parse_worktree_map(output);

        assert_eq!(
            map.get("main"),
            Some(&PathBuf::from("/Users/raine/code/myrepo"))
        );
        assert_eq!(
            map.get("feat-a"),
            Some(&PathBuf::from("/Users/raine/code/myrepo__worktrees/feat-a"))
        );
        assert_eq!(
            map.get("feature/feat-b"),
            Some(&PathBuf::from("/Users/raine/code/myrepo__worktrees/feat-b"))
        );
    }
}
