use anyhow::{Context, Result, bail};
use clap::Parser;
use clap::builder::styling::{AnsiColor, Effects, Styles};
use console::{Color, Style, style};
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
#[command(name = "restack")]
#[command(about = "Rebase stacked PRs onto their current base branches")]
#[command(styles = STYLES)]
struct Cli {
    /// PR numbers to restack (discovers from worktrees if omitted)
    prs: Vec<u32>,

    /// Show what would be done without executing
    #[arg(long)]
    dry_run: bool,

    /// Skip pushing branches after rebasing
    #[arg(long)]
    no_push: bool,
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
        .template("{spinner:.blue} {msg}")
        .unwrap()
}

fn new_spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.enable_steady_tick(Duration::from_millis(120));
    pb.set_style(spinner_style());
    pb.set_message(msg.to_string());
    pb
}

fn finish_spinner(pb: &ProgressBar, msg: &str, ok: bool) {
    let prefix = if ok {
        style("✔").green().bold().to_string()
    } else {
        style("✘").red().bold().to_string()
    };
    pb.finish_and_clear();
    println!("{prefix} {msg}");
}

fn with_spinner<T, F>(msg: &str, op: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    let pb = new_spinner(msg);
    let result = op();
    finish_spinner(&pb, msg, result.is_ok());
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

fn rebase_and_push(dir: &Path, onto: &str, no_push: bool) -> Result<()> {
    run_cmd_in(
        dir,
        Command::new("git").args(["rebase", "--autostash", onto]),
    )
    .with_context(|| {
        format!(
            "resolve conflicts in {} then run: git rebase --continue && git push --force-with-lease",
            dir.display()
        )
    })?;

    if !no_push {
        run_cmd_in(
            dir,
            Command::new("git").args(["push", "--force-with-lease"]),
        )?;
    }

    Ok(())
}

fn rebase_in_temp_worktree(branch: &str, onto: &str, no_push: bool) -> Result<()> {
    let sanitized = branch.replace('/', "-");
    let tmp_dir = std::env::temp_dir().join(format!("restack-{sanitized}"));
    let tmp_str = tmp_dir.to_string_lossy().to_string();

    run_cmd(Command::new("git").args(["worktree", "add", &tmp_str, branch]))
        .with_context(|| format!("failed to create temporary worktree for branch '{branch}'"))?;

    let result = rebase_and_push(&tmp_dir, onto, no_push);

    match &result {
        Ok(()) => {
            let _ = run_cmd(Command::new("git").args(["worktree", "remove", "--force", &tmp_str]));
        }
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("rebase") {
                // Rebase conflict: leave temp worktree for user to resolve
            } else {
                // Other failure (e.g. push): clean up since branch ref is already updated
                let _ =
                    run_cmd(Command::new("git").args(["worktree", "remove", "--force", &tmp_str]));
            }
        }
    }

    result
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

const BRANCH_PALETTE: &[Color] = &[
    Color::Green,
    Color::Cyan,
    Color::Blue,
    Color::Magenta,
    Color::Yellow,
    Color::Red,
];

fn branch_colors(prs: &[PrInfo]) -> HashMap<String, Style> {
    let mut colors = HashMap::new();
    let mut idx = 0;
    for pr in prs {
        for name in [&pr.base_ref, &pr.head_ref] {
            if !colors.contains_key(name.as_str()) {
                colors.insert(
                    name.clone(),
                    Style::new().fg(BRANCH_PALETTE[idx % BRANCH_PALETTE.len()]),
                );
                idx += 1;
            }
        }
    }
    colors
}

fn style_branch<'a>(
    name: &'a str,
    colors: &HashMap<String, Style>,
) -> console::StyledObject<&'a str> {
    match colors.get(name) {
        Some(s) => s.apply_to(name),
        None => Style::new().apply_to(name),
    }
}

struct StackTree {
    roots: Vec<String>,
    children: HashMap<String, Vec<(u32, String)>>, // base_ref -> [(number, head_ref)]
}

impl StackTree {
    fn build(prs: &[PrInfo]) -> Self {
        let head_refs: HashSet<&str> = prs.iter().map(|p| p.head_ref.as_str()).collect();
        let mut children: HashMap<String, Vec<(u32, String)>> = HashMap::new();

        for pr in prs {
            children
                .entry(pr.base_ref.clone())
                .or_default()
                .push((pr.number, pr.head_ref.clone()));
        }

        let mut roots: Vec<String> = prs
            .iter()
            .map(|p| p.base_ref.as_str())
            .filter(|base| !head_refs.contains(base))
            .collect::<HashSet<_>>()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        roots.sort();

        Self { roots, children }
    }

    #[cfg(test)]
    fn format_plain(&self) -> String {
        let mut out = String::new();
        for root in &self.roots {
            out.push_str(&format!("{root}\n"));
            if let Some(kids) = self.children.get(root) {
                self.format_children_plain(kids, "", &mut out);
            }
        }
        out
    }

    #[cfg(test)]
    fn format_children_plain(&self, nodes: &[(u32, String)], prefix: &str, out: &mut String) {
        for (i, (number, head_ref)) in nodes.iter().enumerate() {
            let is_last = i == nodes.len() - 1;
            let connector = if is_last { "└─" } else { "├─" };
            let child_prefix = if is_last { "   " } else { "│  " };

            out.push_str(&format!("{prefix}{connector} #{number} {head_ref}\n"));

            if let Some(kids) = self.children.get(head_ref.as_str()) {
                self.format_children_plain(kids, &format!("{prefix}{child_prefix}"), out);
            }
        }
    }

    fn print_colored(&self, colors: &HashMap<String, Style>) {
        for root in &self.roots {
            println!("{}", style_branch(root, colors).bold());
            if let Some(kids) = self.children.get(root.as_str()) {
                self.print_children_colored(kids, "", colors);
            }
        }
    }

    fn print_children_colored(
        &self,
        nodes: &[(u32, String)],
        prefix: &str,
        colors: &HashMap<String, Style>,
    ) {
        for (i, (number, head_ref)) in nodes.iter().enumerate() {
            let is_last = i == nodes.len() - 1;
            let connector = if is_last { "└─" } else { "├─" };
            let child_prefix = if is_last { "   " } else { "│  " };

            println!(
                "{}{} {} {}",
                style(prefix).dim(),
                style(connector).dim(),
                style(format!("#{number}")).bold(),
                style_branch(head_ref, colors),
            );

            if let Some(kids) = self.children.get(head_ref.as_str()) {
                self.print_children_colored(kids, &format!("{prefix}{child_prefix}"), colors);
            }
        }
    }
}

fn discover_worktree_prs(worktree_map: &HashMap<String, PathBuf>) -> Result<Vec<PrInfo>> {
    let open_prs = with_spinner("Fetching open PRs", get_open_prs)?;
    let mut prs = Vec::new();
    let mut seen = HashSet::new();

    for branch in worktree_map.keys() {
        if let Some(pr) = open_prs.get(branch)
            && seen.insert(pr.number)
        {
            prs.push(pr.clone());
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
        discover_worktree_prs(&worktree_map)?
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

    let prs = sort_by_dependency(prs)?;
    let colors = branch_colors(&prs);
    StackTree::build(&prs).print_colored(&colors);

    if !cli.dry_run {
        with_spinner("Fetching origin", || {
            run_cmd(Command::new("git").args(["fetch", "origin"]))?;
            Ok(())
        })?;
    }

    println!();

    let mut rebased_heads: HashSet<String> = HashSet::new();

    for pr in &prs {
        // Rebase onto local branch if it was just rebased, otherwise onto origin/<base>
        let onto = if rebased_heads.contains(&pr.base_ref) {
            pr.base_ref.clone()
        } else {
            format!("origin/{}", pr.base_ref)
        };

        let onto_styled = if rebased_heads.contains(&pr.base_ref) {
            format!("{}", style_branch(&pr.base_ref, &colors))
        } else {
            format!(
                "{}{}",
                style("origin/").dim(),
                style_branch(&pr.base_ref, &colors)
            )
        };

        let msg = format!(
            "{} {} → {}",
            style(format!("#{}", pr.number)).bold(),
            style_branch(&pr.head_ref, &colors),
            onto_styled,
        );

        if cli.dry_run {
            let push_note = if cli.no_push { "" } else { " + push" };
            println!("  {msg}{push_note}");
        } else {
            let no_push = cli.no_push;
            match worktree_map.get(&pr.head_ref) {
                Some(worktree_path) => {
                    with_spinner(&msg, || rebase_and_push(worktree_path, &onto, no_push))?;
                }
                None => {
                    let head_ref = pr.head_ref.clone();
                    with_spinner(&msg, move || {
                        rebase_in_temp_worktree(&head_ref, &onto, no_push)
                    })?;
                }
            }
        }

        rebased_heads.insert(pr.head_ref.clone());
    }

    if cli.dry_run {
        println!("\n(dry run — no changes made)");
    } else {
        println!("\nAll PRs restacked successfully.");
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
    fn tree_linear_stack() {
        let prs = vec![
            pr(1, "feat-a", "main"),
            pr(2, "feat-b", "feat-a"),
            pr(3, "feat-c", "feat-b"),
        ];
        assert_eq!(
            StackTree::build(&prs).format_plain(),
            "\
main
└─ #1 feat-a
   └─ #2 feat-b
      └─ #3 feat-c\n"
        );
    }

    #[test]
    fn tree_branching_stack() {
        let prs = vec![
            pr(1, "feat-a", "main"),
            pr(2, "feat-b", "main"),
            pr(3, "feat-c", "feat-a"),
        ];
        assert_eq!(
            StackTree::build(&prs).format_plain(),
            "\
main
├─ #1 feat-a
│  └─ #3 feat-c
└─ #2 feat-b\n"
        );
    }

    #[test]
    fn tree_independent_prs() {
        let prs = vec![pr(1, "feat-a", "main"), pr(2, "feat-b", "main")];
        assert_eq!(
            StackTree::build(&prs).format_plain(),
            "\
main
├─ #1 feat-a
└─ #2 feat-b\n"
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
