//! `--profile`: load, save, and check a backup profile, and plan every source
//! it names. The format and its pure checks are
//! [`rdirstat_organize::profile`]; this module adds what needs the
//! filesystem: path resolution, the rules file, and disk identity.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use rdirstat_organize::profile::include_relative;
use rdirstat_organize::{DiskFacts, Profile, Rules, judge_backup_pair, profile_path};

use super::DiskProbe;
use super::plan::{PlanOutcome, PlanRequest, PlanScope, build_plan_scoped};

/// Reads and validates the profile at `path`.
///
/// # Errors
///
/// An unreadable file or a [`rdirstat_organize::ProfileError`].
pub(crate) fn load(path: &Path) -> Result<Profile> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read profile {}", path.display()))?;
    Profile::from_toml(&text).with_context(|| format!("profile {}", path.display()))
}

/// Writes `profile` to `<dest>/.rdirstat/profiles/<name>.toml` through a
/// temp file, `fsync`, and a rename, so a reader sees the old profile or the
/// new one, never half of one. Returns the path written.
///
/// # Errors
///
/// A [`rdirstat_organize::ProfileError`], or an I/O failure.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "the GUI's profile editor (rds-fkk.14.4) is the first caller")
)]
pub(crate) fn save(profile: &Profile) -> Result<PathBuf> {
    use std::io::Write as _;

    let text = profile.to_toml()?;
    let path = profile_path(&profile.dest, &profile.name);
    let Some(parent) = path.parent() else {
        bail!("{} has no parent directory", path.display());
    };
    std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    let temp = parent.join(format!(".{}.toml.tmp", profile.name));
    let mut file = std::fs::File::create(&temp).with_context(|| format!("cannot create {}", temp.display()))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("cannot write {}", temp.display()))?;
    std::fs::rename(&temp, &path).with_context(|| format!("cannot publish {}", path.display()))?;
    Ok(path)
}

/// A profile with every path resolved and checked.
#[derive(Debug)]
pub(crate) struct Resolved {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    /// Canonical destination root.
    pub(crate) dest: PathBuf,
    pub(crate) rules: Rules,
    pub(crate) floor: u64,
    pub(crate) cap: usize,
    pub(crate) sources: Vec<ResolvedSource>,
}

/// One source, resolved.
#[derive(Debug)]
pub(crate) struct ResolvedSource {
    pub(crate) label: String,
    /// Canonical root.
    pub(crate) root: PathBuf,
    /// Relative to `root`, each existing, none inside another. Empty: all.
    pub(crate) includes: Vec<PathBuf>,
}

/// Resolves and checks a loaded profile: the destination and every root are
/// existing directories; the destination is on a different physical disk
/// than every source ([`judge_backup_pair`], the same check `backup plan`
/// makes); every include exists and, after symlinks, is still under its
/// root; the rules file parses. Includes inside another include are dropped,
/// so nothing is planned twice.
///
/// # Errors
///
/// The first check that fails, naming the source.
pub(crate) fn resolve(profile: &Profile, path: &Path, probe: &dyn DiskProbe) -> Result<Resolved> {
    profile.validate()?;
    let dest = canonical_dir(&profile.dest, "destination")?;
    let dest_facts = probe.facts(&dest)?;

    let rules = match &profile.rules {
        Some(rules) => {
            let base = path.parent().unwrap_or_else(|| Path::new("."));
            let rules_path = base.join(rules);
            let text = std::fs::read_to_string(&rules_path)
                .with_context(|| format!("cannot read rules file {}", rules_path.display()))?;
            Rules::from_toml(&text).with_context(|| format!("rules file {}", rules_path.display()))?
        }
        None => Rules::default_backup(),
    };

    let mut sources = Vec::with_capacity(profile.sources.len());
    for source in &profile.sources {
        let root = canonical_dir(&source.root, &format!("source {:?} root", source.label))?;
        let source_facts = probe.facts(&root)?;
        judge_backup_pair(&source_facts, &dest_facts).with_context(|| {
            format!(
                "source {:?}: refusing {} -> {}",
                source.label,
                root.display(),
                dest.display()
            )
        })?;
        let mut includes = Vec::with_capacity(source.include.len());
        for include in &source.include {
            // Lexical first (against the root as written, then as resolved),
            // then the real location, which catches a symlink out of the root.
            let relative = include_relative(&source.root, include)
                .or_else(|| include_relative(&root, include))
                .with_context(|| {
                    format!(
                        "source {:?}: include {} is not under its root",
                        source.label,
                        include.display()
                    )
                })?;
            let absolute = root.join(&relative);
            let real = std::fs::canonicalize(&absolute).with_context(|| {
                format!(
                    "source {:?}: include {} does not exist",
                    source.label,
                    absolute.display()
                )
            })?;
            let Ok(real_relative) = real.strip_prefix(&root) else {
                bail!(
                    "source {:?}: include {} resolves to {}, outside the root {}",
                    source.label,
                    absolute.display(),
                    real.display(),
                    root.display()
                );
            };
            if real_relative.as_os_str().is_empty() {
                bail!(
                    "source {:?}: include {} is the root itself; leave include empty",
                    source.label,
                    absolute.display()
                );
            }
            includes.push(real_relative.to_path_buf());
        }
        includes.sort();
        includes.dedup();
        let mut kept: Vec<PathBuf> = Vec::with_capacity(includes.len());
        for include in includes {
            if !kept.iter().any(|outer| include.starts_with(outer)) {
                kept.push(include);
            }
        }
        sources.push(ResolvedSource {
            label: source.label.clone(),
            root,
            includes: kept,
        });
    }
    Ok(Resolved {
        name: profile.name.clone(),
        path: path.to_path_buf(),
        dest,
        rules,
        floor: profile.floor_allocated,
        cap: profile.review_cap,
        sources,
    })
}

fn canonical_dir(path: &Path, what: &str) -> Result<PathBuf> {
    let resolved =
        std::fs::canonicalize(path).with_context(|| format!("{what} {} cannot be resolved", path.display()))?;
    if !resolved.is_dir() {
        bail!("{what} {} is not a directory", resolved.display());
    }
    Ok(resolved)
}

/// What [`plan_all`] needs beside the profile.
pub(crate) struct PlanAll<'a> {
    pub(crate) catalog: PathBuf,
    pub(crate) app_catalog: Option<PathBuf>,
    pub(crate) progress: bool,
    pub(crate) rollup: bool,
    /// Write nothing (the recommendation).
    pub(crate) dry_run: bool,
    pub(crate) probe: &'a dyn DiskProbe,
}

/// Plans every source of `profile`, in order.
///
/// Every source shares one destination, so each plan's budget is what the
/// earlier plans left: the destination's free space is reduced by the logical
/// bytes already planned before the next source is fitted. Together the
/// plans keep the destination under the fill rule, not just each one alone.
///
/// # Errors
///
/// The first source whose plan fails.
pub(crate) fn plan_all(profile: &Resolved, options: &PlanAll<'_>) -> Result<Vec<PlanOutcome>> {
    let probe = Reserving {
        inner: options.probe,
        dest: profile.dest.clone(),
        reserved: Cell::new(0),
    };
    let mut outcomes = Vec::with_capacity(profile.sources.len());
    for source in &profile.sources {
        let request = PlanRequest {
            source: Some(source.root.clone()),
            label: source.label.clone(),
            dest: profile.dest.clone(),
            floor: profile.floor,
            snapshot: None,
            rules: profile.rules.clone(),
            out: None,
            catalog: options.catalog.clone(),
            app_catalog: options.app_catalog.clone(),
            progress: options.progress,
            rollup: options.rollup,
        };
        let scope = PlanScope {
            includes: source.includes.clone(),
            cap: profile.cap,
            dry_run: options.dry_run,
        };
        let outcome = build_plan_scoped(&request, &scope, &probe)
            .with_context(|| format!("profile {:?}, source {:?}", profile.name, source.label))?;
        probe
            .reserved
            .set(probe.reserved.get().saturating_add(outcome.total_logical));
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// A probe that reports the destination with `reserved` bytes already spent.
struct Reserving<'a> {
    inner: &'a dyn DiskProbe,
    dest: PathBuf,
    reserved: Cell<u64>,
}

impl DiskProbe for Reserving<'_> {
    fn facts(&self, path: &Path) -> Result<DiskFacts> {
        let mut facts = self.inner.facts(path)?;
        if path == self.dest {
            facts.available_bytes = facts.available_bytes.saturating_sub(self.reserved.get());
        }
        Ok(facts)
    }
}

/// `backup plan --profile <path>`: writes one plan file per source and
/// prints each plan's report (text), or `{ "profile", "plans": [...] }`.
pub(crate) fn run_plan(path: &Path, args: &crate::cli::BackupPlanArgs) -> Result<std::process::ExitCode> {
    let profile = resolve(&load(path)?, path, &super::SystemProbe)?;
    let outcomes = plan_all(
        &profile,
        &PlanAll {
            catalog: super::catalog_dir(args.catalog.as_deref())?,
            app_catalog: super::catalog_dir(None).ok(),
            progress: crate::wants_progress(args.progress),
            rollup: !args.no_rollup,
            dry_run: false,
            probe: &super::SystemProbe,
        },
    )?;
    let text = if args.json {
        let doc = serde_json::json!({
            "profile": profile.name,
            "profile_path": profile.path.to_string_lossy(),
            "plans": outcomes.iter().map(|o| super::plan::json(o, args.top)).collect::<Vec<_>>(),
        });
        let mut text = serde_json::to_string_pretty(&doc)?;
        text.push('\n');
        text
    } else {
        outcomes
            .iter()
            .map(|o| super::plan::text(o, args.top))
            .collect::<Vec<_>>()
            .join("\n")
    };
    super::print(&text)?;
    Ok(std::process::ExitCode::SUCCESS)
}

/// `backup check-profile`: [`resolve`] alone, reported as a
/// [`rdirstat_organize::ProfileCheck`].
pub(crate) fn run_check(args: &crate::cli::BackupProfileArgs) -> Result<std::process::ExitCode> {
    let resolved = resolve(&load(&args.profile)?, &args.profile, &super::SystemProbe)?;
    let check = check_of(&resolved);
    let text = if args.json {
        let mut text = serde_json::to_string(&check)?;
        text.push('\n');
        text
    } else {
        use std::fmt::Write as _;
        let mut text = format!("Profile {} -> {}: ok\n", check.name, check.dest.display());
        for source in &check.sources {
            let _ = writeln!(
                text,
                "  {}  {}  ({} includes)",
                source.label,
                source.root.display(),
                source.includes.len()
            );
        }
        text
    };
    super::print(&text)?;
    Ok(std::process::ExitCode::SUCCESS)
}

/// The report of a resolved profile.
pub(crate) fn check_of(resolved: &Resolved) -> rdirstat_organize::ProfileCheck {
    rdirstat_organize::ProfileCheck {
        name: resolved.name.clone(),
        dest: resolved.dest.clone(),
        floor_allocated: resolved.floor,
        review_cap: resolved.cap,
        sources: resolved
            .sources
            .iter()
            .map(|source| rdirstat_organize::ProfileCheckSource {
                label: source.label.clone(),
                root: source.root.clone(),
                includes: source.includes.clone(),
            })
            .collect(),
    }
}

/// `backup recommend --profile <path>`.
pub(crate) fn run_recommend(args: &crate::cli::BackupRecommendArgs) -> Result<std::process::ExitCode> {
    let profile = resolve(&load(&args.profile)?, &args.profile, &super::SystemProbe)?;
    let doc = super::recommend::recommend(
        &profile,
        args.limit,
        crate::wants_progress(args.progress),
        &super::SystemProbe,
    )?;
    let text = if args.json {
        let mut text = serde_json::to_string_pretty(&doc)?;
        text.push('\n');
        text
    } else {
        super::recommend::text(&doc)
    };
    super::print(&text)?;
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test code: a failed fixture must abort loudly"
    )]

    use std::path::{Path, PathBuf};

    use anyhow::Result;
    use rdirstat_organize::{DiskFacts, Profile, ProfileSource, RowStatus};

    use super::{DiskProbe, PlanAll, load, plan_all, resolve, save};
    use crate::backup::recommend;

    const RULES: &str = r#"
version = 1

[[rule]]
id = "caches"
reason = "rebuildable cache"
action = "skip"
globs = ["**/cache"]
"#;

    /// Paths under any of `sources` are on `source_disk`; the rest on disk14.
    struct Probe {
        sources: Vec<PathBuf>,
        source_disk: &'static str,
        total: u64,
        available: u64,
    }

    impl DiskProbe for Probe {
        fn facts(&self, path: &Path) -> Result<DiskFacts> {
            let on_source = self.sources.iter().any(|root| path.starts_with(root));
            Ok(DiskFacts {
                disk_id: Some(if on_source { self.source_disk } else { "disk14" }.to_owned()),
                volume_uuid: Some(if on_source { "SRC" } else { "DEST-UUID" }.to_owned()),
                st_dev: if on_source { 1 } else { 2 },
                filesystem: "apfs".into(),
                total_bytes: self.total,
                available_bytes: self.available,
            })
        }
    }

    struct Fixture {
        _dirs: Vec<tempfile::TempDir>,
        a: PathBuf,
        b: PathBuf,
        dest: PathBuf,
        catalog: PathBuf,
    }

    fn put(root: &Path, relative: &str, len: usize) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![7_u8; len]).unwrap();
    }

    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path()).unwrap();
        (dir, path)
    }

    impl Fixture {
        fn new() -> Self {
            let (da, a) = canonical_tempdir();
            let (db, b) = canonical_tempdir();
            let (dd, dest) = canonical_tempdir();
            let (dc, catalog) = canonical_tempdir();
            put(&a, "docs/big.bin", 256 << 10);
            put(&a, "docs/cache/huge.bin", 512 << 10);
            put(&a, "docs/small.bin", 64 << 10);
            put(&a, "music/song.bin", 128 << 10);
            put(&a, "outside/ignored.bin", 1 << 20);
            put(&a, "disk.iso", 192 << 10);
            put(&b, "photos/p.bin", 320 << 10);
            std::fs::write(dest.join("rules.toml"), RULES).unwrap();
            Self {
                _dirs: vec![da, db, dd, dc],
                a,
                b,
                dest,
                catalog,
            }
        }

        fn profile(&self) -> Profile {
            Profile {
                schema_version: rdirstat_organize::PROFILE_SCHEMA_VERSION,
                name: "laptop".into(),
                dest: self.dest.clone(),
                rules: Some(PathBuf::from("../../rules.toml")),
                floor_allocated: 32 << 10,
                review_cap: 100,
                sources: vec![
                    ProfileSource {
                        label: "a".into(),
                        root: self.a.clone(),
                        include: vec![
                            PathBuf::from("docs"),
                            self.a.join("disk.iso"),
                            PathBuf::from("docs/cache"),
                        ],
                    },
                    ProfileSource {
                        label: "b".into(),
                        root: self.b.clone(),
                        include: Vec::new(),
                    },
                ],
            }
        }

        fn probe(&self) -> Probe {
            Probe {
                sources: vec![self.a.clone(), self.b.clone()],
                source_disk: "disk3",
                total: 1 << 40,
                available: 1 << 40,
            }
        }

        fn options<'a>(&self, probe: &'a Probe, dry_run: bool) -> PlanAll<'a> {
            PlanAll {
                catalog: self.catalog.clone(),
                app_catalog: None,
                progress: false,
                rollup: true,
                dry_run,
                probe,
            }
        }
    }

    fn tree(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path.clone());
                }
                out.push(path);
            }
        }
        out.sort();
        out
    }

    #[test]
    fn a_profile_is_saved_on_the_drive_atomically_and_loads_back() {
        let fx = Fixture::new();
        let path = save(&fx.profile()).expect("save");
        assert_eq!(path, fx.dest.join(".rdirstat/profiles/laptop.toml"));
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            names,
            vec![std::ffi::OsString::from("laptop.toml")],
            "no temp file left"
        );
        assert_eq!(load(&path).expect("load"), fx.profile());
    }

    #[test]
    fn resolve_refuses_a_destination_on_a_source_disk() {
        let fx = Fixture::new();
        let path = save(&fx.profile()).unwrap();
        let mut probe = fx.probe();
        probe.source_disk = "disk14";
        let error = resolve(&fx.profile(), &path, &probe).expect_err("same disk");
        assert!(format!("{error:#}").contains("source \"a\""), "{error:#}");
    }

    #[test]
    fn resolve_checks_includes_and_drops_nested_ones() {
        let fx = Fixture::new();
        let path = save(&fx.profile()).unwrap();
        let resolved = resolve(&fx.profile(), &path, &fx.probe()).expect("resolve");
        assert_eq!(
            resolved.sources[0].includes,
            vec![PathBuf::from("disk.iso"), PathBuf::from("docs")]
        );
        assert!(resolved.sources[1].includes.is_empty());
        let check = super::check_of(&resolved);
        assert_eq!(check.name, resolved.name);
        assert_eq!(check.dest, resolved.dest);
        assert_eq!(check.sources[0].includes, resolved.sources[0].includes);
        let text = serde_json::to_string(&check).unwrap();
        assert_eq!(
            serde_json::from_str::<rdirstat_organize::ProfileCheck>(&text).unwrap(),
            check
        );

        let mut missing = fx.profile();
        missing.sources[0].include = vec![PathBuf::from("nope")];
        assert!(format!("{:#}", resolve(&missing, &path, &fx.probe()).unwrap_err()).contains("does not exist"));

        std::os::unix::fs::symlink(&fx.b, fx.a.join("escape")).unwrap();
        let mut escape = fx.profile();
        escape.sources[0].include = vec![PathBuf::from("escape")];
        assert!(format!("{:#}", resolve(&escape, &path, &fx.probe()).unwrap_err()).contains("outside the root"));
    }

    #[test]
    fn plan_profile_plans_every_source_narrowed_to_its_includes() {
        let fx = Fixture::new();
        let path = save(&fx.profile()).unwrap();
        let resolved = resolve(&fx.profile(), &path, &fx.probe()).unwrap();
        let probe = fx.probe();
        let outcomes = plan_all(&resolved, &fx.options(&probe, false)).expect("plan");
        assert_eq!(outcomes.len(), 2);

        let a = &outcomes[0];
        assert_eq!(a.plan.source_root, fx.a);
        assert!(a.plan_path.exists(), "plan file written");
        let planned: Vec<_> = a.plan.items.iter().map(|i| i.relative.clone()).collect();
        assert!(planned.contains(&PathBuf::from("docs/big.bin")), "{planned:?}");
        assert!(
            planned.contains(&PathBuf::from("disk.iso")),
            "a named file is a candidate: {planned:?}"
        );
        for relative in &planned {
            assert!(
                relative.starts_with("docs") || relative == Path::new("disk.iso"),
                "{} is outside the includes",
                relative.display()
            );
        }
        assert!(
            a.plan.skipped.iter().any(|s| s.rule_id == "caches"),
            "the profile's rules file applies"
        );

        let b = &outcomes[1];
        assert!(b.plan.items.iter().any(|i| i.relative == Path::new("photos/p.bin")));
    }

    #[test]
    fn recommend_ranks_rows_writes_nothing_and_judges_the_fill() {
        let fx = Fixture::new();
        let path = save(&fx.profile()).unwrap();
        let resolved = resolve(&fx.profile(), &path, &fx.probe()).unwrap();
        let before_dest = tree(&fx.dest);
        let before_a = tree(&fx.a);

        let doc = recommend::recommend(&resolved, 1000, false, &fx.probe()).expect("recommend");
        assert_eq!(tree(&fx.dest), before_dest, "nothing written on the destination");
        assert_eq!(tree(&fx.a), before_a, "nothing written on the source");
        assert!(tree(&fx.catalog).is_empty(), "no catalog part written");

        assert_eq!(doc.sources.len(), 2);
        assert!(doc.fill.fits);
        assert!(!doc.rows.is_empty());
        assert!(
            doc.rows.windows(2).all(|w| w[0].allocated >= w[1].allocated),
            "largest first"
        );
        assert!(doc.rows.iter().enumerate().all(|(i, r)| r.rank == i as u64 + 1));
        let skipped = doc
            .rows
            .iter()
            .find(|r| r.status == RowStatus::Skipped)
            .expect("a skipped row");
        assert_eq!(skipped.rule_id.as_deref(), Some("caches"));
        assert_eq!(skipped.reason.as_deref(), Some("rebuildable cache"));
        assert!(doc.rows.iter().all(|r| !r.relative.starts_with("outside")));
        assert_eq!(
            doc.totals.planned_items,
            doc.sources.iter().map(|s| s.totals.planned_items).sum::<u64>()
        );

        let json = serde_json::to_value(&doc).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["fill"]["fits"], true);

        let limited = recommend::recommend(&resolved, 1, false, &fx.probe()).unwrap();
        assert_eq!(limited.rows.len(), 1);
        assert!(limited.rows_truncated);
    }

    #[test]
    fn sources_share_the_destination_budget() {
        let fx = Fixture::new();
        let path = save(&fx.profile()).unwrap();
        let resolved = resolve(&fx.profile(), &path, &fx.probe()).unwrap();
        // 0.75 * 1 MiB = 768 KiB of room: source a's includes take most of it,
        // so b's 320 KiB photo cannot also fit.
        let probe = Probe {
            total: 1 << 20,
            available: 1 << 20,
            ..fx.probe()
        };
        let outcomes = plan_all(&resolved, &fx.options(&probe, true)).unwrap();
        let a_logical = outcomes[0].total_logical;
        assert!(
            a_logical > (768 << 10) - (320 << 10),
            "fixture: a leaves too little for b ({a_logical})"
        );
        assert!(
            outcomes[1]
                .deferred
                .iter()
                .any(|i| i.relative == Path::new("photos/p.bin")),
            "b is fitted after a"
        );
        let doc = recommend::build(&resolved, &outcomes, 1 << 20, 1 << 20, 1000);
        assert!(!doc.fill.fits);
        assert!(doc.rows.iter().any(|r| r.status == RowStatus::Deferred));
    }
}
