use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    path::{Path, PathBuf},
    process::Command,
};

use codgrep::{QueryConfig, SearchEngine, SearchMode, SearchPolicy};

const RG_CHUNK_SIZE: usize = 512;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut repo: Option<PathBuf> = None;
    let mut index: Option<PathBuf> = None;
    let mut pattern: Option<String> = None;
    let mut case_insensitive = false;
    let mut limit = 20usize;
    let mut whole_repo_rg = false;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--repo" => repo = args.next().map(PathBuf::from),
            "--index" => index = args.next().map(PathBuf::from),
            "--pattern" => pattern = args.next(),
            "--limit" => {
                let value = args.next().ok_or("--limit requires a value")?;
                limit = value.parse::<usize>()?;
            }
            "--whole-repo-rg" => whole_repo_rg = true,
            "-i" | "--ignore-case" => case_insensitive = true,
            other => return Err(format!("unknown arg: {other}").into()),
        }
    }

    let repo = repo.ok_or("--repo is required")?;
    let index = index.unwrap_or_else(|| repo.join(".codgrep-index-engine"));
    let pattern = pattern.ok_or("--pattern is required")?;

    let engine = SearchEngine::new(codgrep::BuildConfig {
        repo_path: repo.clone(),
        storage_root: index,
        ..codgrep::BuildConfig::default()
    });
    let config = QueryConfig {
        regex_pattern: pattern.clone(),
        patterns: vec![pattern.clone()],
        case_insensitive,
        search_mode: SearchMode::CountOnly,
        ..QueryConfig::default()
    };

    let codgrep_counts = engine
        .count_matches_by_file_including_zero(&config, None)?
        .into_iter()
        .map(|count| (count.path, count.matched_lines))
        .collect::<BTreeMap<_, _>>();
    let visible_paths = engine.visible_paths(None)?;
    let total = engine
        .search(&config, SearchPolicy::IndexOnly)?
        .results
        .matched_lines;
    let rg_counts = run_rg_count_by_file(&repo, &pattern, case_insensitive, Some(&visible_paths))?;

    let paths = codgrep_counts
        .keys()
        .cloned()
        .chain(rg_counts.keys().cloned())
        .collect::<BTreeSet<_>>();

    let codgrep_total = total;
    let rg_total: usize = rg_counts.values().sum();
    println!("visible_files={}", visible_paths.len());
    println!("codgrep_total={codgrep_total}");
    println!("rg_total={rg_total}");

    let mut diff_count = 0usize;
    for path in paths {
        let codgrep = codgrep_counts.get(&path).copied().unwrap_or_default();
        let rg = rg_counts.get(&path).copied().unwrap_or_default();
        if codgrep == rg {
            continue;
        }
        diff_count += 1;
        if diff_count <= limit {
            println!("{path}\tcodgrep={codgrep}\trg={rg}");
        }
    }
    println!("diff_files={diff_count}");

    if whole_repo_rg {
        let whole_repo_counts =
            run_rg_count_by_file(repo.as_path(), &pattern, case_insensitive, None)?;
        let whole_repo_total: usize = whole_repo_counts.values().sum();
        println!("whole_repo_rg_total={whole_repo_total}");

        let visible_path_set = visible_paths.into_iter().collect::<BTreeSet<_>>();
        let mut rg_only_files = 0usize;
        let mut rg_only_total = 0usize;
        for (path, count) in whole_repo_counts {
            if visible_path_set.contains(&path) || count == 0 {
                continue;
            }
            rg_only_files += 1;
            rg_only_total += count;
            if rg_only_files <= limit {
                println!("rg_only\t{path}\tcount={count}");
            }
        }
        println!("rg_only_files={rg_only_files}");
        println!("rg_only_total={rg_only_total}");
    }

    Ok(())
}

fn run_rg_count_by_file(
    repo: &Path,
    pattern: &str,
    case_insensitive: bool,
    visible_paths: Option<&[String]>,
) -> Result<BTreeMap<String, usize>, Box<dyn std::error::Error>> {
    let mut counts = BTreeMap::new();
    let chunks: Vec<Vec<String>> = if let Some(visible_paths) = visible_paths {
        visible_paths
            .chunks(RG_CHUNK_SIZE)
            .map(|chunk| chunk.to_vec())
            .collect()
    } else {
        vec![Vec::new()]
    };
    for chunk in chunks {
        let mut command = Command::new("rg");
        command
            .current_dir(repo)
            .arg("--color")
            .arg("never")
            .arg("--no-heading")
            .arg("--with-filename")
            .arg("--count")
            .arg("--no-messages");
        if case_insensitive {
            command.arg("--ignore-case");
        }
        command.arg("-e").arg(pattern).arg("--");
        if chunk.is_empty() {
            command.arg(".");
        }
        for path in &chunk {
            command.arg(path);
        }

        let output = command.output()?;
        if !output.status.success() && output.status.code() != Some(1) {
            return Err(format!(
                "rg failed with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }

        for line in String::from_utf8(output.stdout)?.lines() {
            let Some((path, count)) = line.rsplit_once(':') else {
                continue;
            };
            let path = path.strip_prefix("./").unwrap_or(path);
            let path = repo.join(path);
            counts.insert(path.to_string_lossy().into_owned(), count.parse::<usize>()?);
        }
    }
    Ok(counts)
}
