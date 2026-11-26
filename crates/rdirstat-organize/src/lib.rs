//! Pure planning for backup, migrate, and organize: largest-first fit, disk
//! identity, skip rules, mirror layout, plan tokens.
//!
//! This crate is pure (.settings/docs/11-ORGANIZE.md, "Where the code goes"
//! and Amendment A). It does not depend on Tauri, spawn processes, or touch
//! a filesystem. The shell hands it scan items and disk facts and performs
//! every syscall itself, through `rdirstat-copy`.
//!
//! | Module | Owns |
//! | --- | --- |
//! | [`rules`] | the versioned TOML skip/include rules and their hash |
//! | [`pair`] | refusing a source/destination pair that is not a backup |
//! | [`layout`] | `<dest>/Backups/<label>/<relative>` |
//! | [`fit`] | largest-first wave selection under the destination reserve |
//! | [`plan`] | the JSON plan file and its SHA-256 token |

#![forbid(unsafe_code)]

pub mod fit;
pub mod layout;
pub mod pair;
pub mod plan;
pub mod rules;

pub use fit::{
    DEFAULT_CAP, DEFAULT_FLOOR_ALLOCATED, DEFAULT_RESERVE_PRESSURE, FitInput, FitPlan, ItemKind, PlanItem,
    RULE_NON_UTF8, Rollup, RollupScope, ScanItem, SkippedItem, Warning, fit,
};
pub use layout::{BACKUPS_DIR, LayoutError, mirror_path, validate_label};
pub use pair::{DiskFacts, PairRefusal, judge_backup_pair};
pub use plan::{INTENT_BACKUP, PLAN_SCHEMA_VERSION, PlanFile, plan_token};
pub use rules::{Rule, RuleAction, Rules, RulesError};
