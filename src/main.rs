use anyhow::{Context, Result, bail};
use clap::Parser;
use clap::builder::styling::{AnsiColor, Effects, Styles};
use indicatif::{ProgressBar, ProgressStyle};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default());

#[derive(Parser)]
#[command(name = "gh-restack")]
#[command(about = "Rebase stacked PRs onto their current base branches")]
#[command(styles = STYLES)]
struct Cli {
    /// PR numbers to restack (auto-discovers from worktrees if omitted)
    prs: Vec<u32>,

    /// Show what would be done without executing
    #[arg(long)]
    dry_run: bool,

    /// Skip pushing branches after rebasing
    #[arg(long)]
    no_push: bool,
}

fn with_spinner<T, F>(msg: &str, op: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    let pb = ProgressBar::new_spinner();
    pb.enable_steady_tick(Duration::from_millis(120));
    pb.set_style(
        ProgressStyle::default_spinner()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
            .template("{spinner:.blue} {msg}")
            .unwrap(),
    );
    pb.set_message(msg.to_string());
    let result = op();
    match &result {
        Ok(_) => pb.finish_with_message(format!("✔ {msg}")),
        Err(_) => pb.finish_with_message(format!("✘ {msg}")),
    }
    result
}

#[derive(Deserialize, Debug, Clone)]
struct PrInfo {
    number: u32,
    #[serde(rename = "headRefName")]
    head_ref: String,
    #[serde(rename = "baseRefName")]
    base_ref: String,
    state: String,
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

fn get_pr_info(id: &str) -> Result<PrInfo> {
    let output = run_cmd(Command::new("gh").args([
        "pr",
        "view",
        id,
        "--json",
        "number,headRefName,baseRefName,state",
    ]))
    .with_context(|| format!("failed to get info for PR {id}"))?;
    serde_json::from_str(&output).with_context(|| format!("failed to parse PR {id} info"))
}

fn get_open_prs() -> Result<HashMap<String, PrInfo>> {
    let output = run_cmd(Command::new("gh").args([
        "pr",
        "list",
        "--state",
        "open",
        "--limit",
        "100",
        "--json",
        "number,headRefName,baseRefName,state",
    ]))
    .context("failed to list open PRs")?;
    let prs: Vec<PrInfo> =
        serde_json::from_str(&output).context("failed to parse open PRs list")?;
    Ok(prs.into_iter().map(|p| (p.head_ref.clone(), p)).collect())
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
fn sort_by_dependency(prs: Vec<PrInfo>) -> Result<Vec<PrInfo>> {
    let mut remaining = prs;
    let mut sorted = Vec::with_capacity(remaining.len());

    while !remaining.is_empty() {
        let remaining_heads: HashSet<&str> =
            remaining.iter().map(|p| p.head_ref.as_str()).collect();

        let pos = remaining
            .iter()
            .position(|pr| !remaining_heads.contains(pr.base_ref.as_str()));

        match pos {
            Some(idx) => sorted.push(remaining.remove(idx)),
            None => {
                let cycle_prs: Vec<String> = remaining
                    .iter()
                    .map(|p| format!("PR #{} ({} → {})", p.number, p.head_ref, p.base_ref))
                    .collect();
                bail!(
                    "circular dependency detected among PRs:\n  {}",
                    cycle_prs.join("\n  ")
                );
            }
        }
    }

    Ok(sorted)
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

fn discover_worktree_prs(worktree_map: &HashMap<String, PathBuf>) -> Result<Vec<PrInfo>> {
    let open_prs = get_open_prs()?;
    let mut prs = Vec::new();
    let mut seen = HashSet::new();

    for branch in worktree_map.keys() {
        // Fast path: found in bulk list
        if let Some(pr) = open_prs.get(branch) {
            if seen.insert(pr.number) {
                prs.push(pr.clone());
            }
        } else {
            // Slow path: branch might have a PR not in the top 100 results.
            // Errors are ignored since most branches (e.g. main) won't have PRs.
            if let Ok(pr) = get_pr_info(branch)
                && pr.state == "OPEN"
                && seen.insert(pr.number)
            {
                prs.push(pr);
            }
        }
    }

    if prs.is_empty() {
        bail!("no open PRs found for checked-out worktree branches");
    }

    Ok(prs)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let worktree_map = get_worktree_map()?;

    let prs = if cli.prs.is_empty() {
        with_spinner("Discovering PRs from worktrees", || {
            discover_worktree_prs(&worktree_map)
        })?
    } else {
        let mut seen = HashSet::new();
        let pr_numbers: Vec<u32> = cli.prs.into_iter().filter(|n| seen.insert(*n)).collect();
        let mut prs = Vec::new();
        for pr_number in &pr_numbers {
            let info = get_pr_info(&pr_number.to_string())?;
            if info.state != "OPEN" {
                bail!(
                    "PR #{} is {}, not open",
                    info.number,
                    info.state.to_lowercase()
                );
            }
            prs.push(info);
        }
        prs
    };

    for pr in &prs {
        println!("PR #{}: {} → {}", pr.number, pr.head_ref, pr.base_ref);
    }

    let prs = sort_by_dependency(prs)?;

    // Preflight: verify all branches are checked out and worktrees are clean
    for pr in &prs {
        if !worktree_map.contains_key(&pr.head_ref) {
            bail!(
                "branch '{}' (PR #{}) is not checked out in any worktree",
                pr.head_ref,
                pr.number
            );
        }
    }

    if !cli.dry_run {
        for pr in &prs {
            let worktree_path = &worktree_map[&pr.head_ref];
            check_worktree_clean(worktree_path)
                .with_context(|| format!("PR #{} ({})", pr.number, worktree_path.display()))?;
        }

        with_spinner("Fetching origin", || {
            run_cmd(Command::new("git").args(["fetch", "origin"]))?;
            Ok(())
        })?;
    }

    println!();

    let mut rebased_heads: HashMap<String, PathBuf> = HashMap::new();

    for pr in &prs {
        let worktree_path = &worktree_map[&pr.head_ref];

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
            if !cli.no_push {
                println!(
                    "PR #{}: would push '{}' (force-with-lease)",
                    pr.number, pr.head_ref
                );
            }
        } else {
            with_spinner(
                &format!(
                    "PR #{}: rebasing '{}' onto '{}'",
                    pr.number, pr.head_ref, onto
                ),
                || {
                    run_cmd_in(worktree_path, Command::new("git").args(["rebase", &onto]))
                        .with_context(|| {
                            format!(
                                "resolve conflicts in {} then run: git rebase --continue",
                                worktree_path.display()
                            )
                        })
                },
            )?;

            if !cli.no_push {
                with_spinner(
                    &format!("PR #{}: pushing '{}'", pr.number, pr.head_ref),
                    || {
                        run_cmd_in(
                            worktree_path,
                            Command::new("git").args(["push", "--force-with-lease"]),
                        )
                    },
                )?;
            }
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
            state: "OPEN".to_string(),
        }
    }

    #[test]
    fn sort_independent_prs_preserves_order() {
        let prs = vec![pr(1, "feat-a", "main"), pr(2, "feat-b", "main")];
        let sorted = sort_by_dependency(prs).unwrap();
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
        let sorted = sort_by_dependency(prs).unwrap();
        assert_eq!(sorted[0].number, 1);
        assert_eq!(sorted[1].number, 2);
        assert_eq!(sorted[2].number, 3);
    }

    #[test]
    fn sort_partial_stack() {
        let prs = vec![pr(3, "feat-c", "feat-b"), pr(2, "feat-b", "main")];
        let sorted = sort_by_dependency(prs).unwrap();
        assert_eq!(sorted[0].number, 2);
        assert_eq!(sorted[1].number, 3);
    }

    #[test]
    fn sort_detects_cycle() {
        let prs = vec![pr(1, "feat-a", "feat-b"), pr(2, "feat-b", "feat-a")];
        let result = sort_by_dependency(prs);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("circular dependency")
        );
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
