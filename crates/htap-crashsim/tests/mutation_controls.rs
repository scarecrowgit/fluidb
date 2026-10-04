//! Mutation-control coverage checks.
//!
//! The scanner intentionally does not resolve UFCS calls, module or crate aliases, glob imports,
//! renamed site functions, calls inside macro invocations, or helpers reached through another
//! crate's public wrapper. Raw `std`/`libc` fsync calls are separately forbidden by the workspace
//! Clippy durability gate in `ci/clippy-durability`.

mod mutation_controls_scan;
mod mutation_controls_tables;

use mutation_controls_scan::{scan, HelperCall, MacroUse, ScanResult, SiteUse};
use mutation_controls_tables::{
    AllowEntry, AllowReason, SurvivorProof, ALLOWLIST, CONTROL_EXEMPTIONS, HELPER_SITES,
    SYNC_SITE_WITNESSES, UNATTRIBUTED_WITNESSES,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

fn fail_with_errors(test_name: &str, errors: Vec<String>) {
    if !errors.is_empty() {
        panic!(
            "{test_name} found {} error(s):\n{}",
            errors.len(),
            errors
                .into_iter()
                .map(|error| format!("- {error}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

fn observed_sites(scan: &ScanResult) -> BTreeSet<&str> {
    scan.sites.iter().map(|site| site.site.as_str()).collect()
}

fn distinct_helper_calls(scan: &ScanResult) -> BTreeSet<(&str, &str)> {
    scan.helper_calls
        .iter()
        .map(|HelperCall { crate_name, helper }| (crate_name.as_str(), helper.as_str()))
        .collect()
}

fn matching_macro<'a>(
    macros: &'a [MacroUse],
    kind: &str,
    test_file: &str,
    fn_name: &str,
    site: Option<&str>,
) -> Option<&'a MacroUse> {
    macros.iter().find(|macro_use| {
        macro_use.kind == kind
            && macro_use.file == Path::new(test_file)
            && macro_use.fn_name == fn_name
            && macro_use.site.as_deref() == site
    })
}

fn reason_name(reason: &AllowReason) -> &'static str {
    match reason {
        AllowReason::SubsumedByLaterSync => "SubsumedByLaterSync",
        AllowReason::CoveredByOtherSync => "CoveredByOtherSync",
        AllowReason::IdempotentResurrection => "IdempotentResurrection",
        AllowReason::ErrorPathOnly => "ErrorPathOnly",
        AllowReason::NonUnixOnly => "NonUnixOnly",
        AllowReason::NoProductionCaller => "NoProductionCaller",
        AllowReason::OracleGap => "OracleGap",
    }
}

fn allow_entry_name(entry: &AllowEntry) -> String {
    match entry.crate_scope {
        Some(crate_name) => format!(
            "{} scoped to {} ({})",
            entry.site,
            crate_name,
            reason_name(&entry.reason)
        ),
        None => format!("{} globally ({})", entry.site, reason_name(&entry.reason)),
    }
}

fn is_powerloss_suite(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("powerloss") && name.ends_with(".rs"))
        && path
            .components()
            .any(|component| component.as_os_str() == "tests")
}

fn site_use_description(site: &SiteUse) -> String {
    format!(
        "{} at {}:{} in {}",
        site.site,
        site.file.display(),
        site.line,
        site.crate_name
    )
}

#[test]
fn every_observed_sync_site_has_a_killing_witness_or_a_reasoned_allowlist_entry() {
    let result = scan();
    let witnessed: BTreeSet<&str> = SYNC_SITE_WITNESSES
        .iter()
        .map(|(site, _, _, _)| *site)
        .collect();
    let globally_allowed: BTreeSet<&str> = ALLOWLIST
        .iter()
        .filter(|entry| entry.crate_scope.is_none())
        .map(|entry| entry.site)
        .collect();

    let mut missing = BTreeMap::<&str, Vec<&SiteUse>>::new();
    for site_use in &result.sites {
        if !witnessed.contains(site_use.site.as_str())
            && !globally_allowed.contains(site_use.site.as_str())
        {
            missing
                .entry(site_use.site.as_str())
                .or_default()
                .push(site_use);
        }
    }

    let errors = missing
        .into_iter()
        .map(|(site, uses)| {
            let locations = uses
                .into_iter()
                .map(site_use_description)
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "observed site {site:?} has neither a witness nor a global allowlist entry; uses: {locations}"
            )
        })
        .collect();

    fail_with_errors(
        "every_observed_sync_site_has_a_killing_witness_or_a_reasoned_allowlist_entry",
        errors,
    );
}

#[test]
fn allowlist_entries_are_not_stale() {
    let result = scan();
    let sites = observed_sites(&result);
    let witnessed: BTreeSet<&str> = SYNC_SITE_WITNESSES
        .iter()
        .map(|(site, _, _, _)| *site)
        .collect();
    let helper_calls = distinct_helper_calls(&result);
    let mut errors = Vec::new();

    for entry in ALLOWLIST {
        let entry_name = allow_entry_name(entry);

        if !sites.contains(entry.site) {
            errors.push(format!(
                "allowlist entry {entry_name} is stale because the site is not observed"
            ));
        }

        if entry.crate_scope.is_none() && witnessed.contains(entry.site) {
            errors.push(format!(
                "global allowlist entry {entry_name} also has a witness row"
            ));
        }

        if let Some(crate_name) = entry.crate_scope {
            let has_matching_helper_call = HELPER_SITES.iter().any(|(helper, site)| {
                *site == entry.site && helper_calls.contains(&(crate_name, *helper))
            });
            if !has_matching_helper_call {
                errors.push(format!(
                    "scoped allowlist entry {entry_name} is stale because crate {crate_name:?} \
                     does not call a helper mapped to site {:?}",
                    entry.site
                ));
            }
        }

        for covered_by in entry.covered_by {
            if !sites.contains(covered_by) {
                errors.push(format!(
                    "allowlist entry {entry_name} references unobserved covered_by site {covered_by:?}"
                ));
            }
        }

        if !entry.covered_by.is_empty()
            && !entry
                .covered_by
                .iter()
                .any(|covered_by| witnessed.contains(covered_by))
        {
            errors.push(format!(
                "allowlist entry {entry_name} has no covered_by site with a killing witness"
            ));
        }

        if matches!(entry.reason, AllowReason::OracleGap) && !entry.covered_by.is_empty() {
            errors.push(format!(
                "OracleGap allowlist entry {entry_name} must have empty covered_by"
            ));
        }

        match &entry.proof {
            SurvivorProof::Macro {
                test_file,
                survivor_fn,
            } => {
                if matching_macro(
                    &result.macros,
                    "crashsim_survivor",
                    test_file,
                    survivor_fn,
                    Some(entry.site),
                )
                .is_none()
                {
                    errors.push(format!(
                        "allowlist entry {entry_name} has no matching crashsim_survivor! \
                         in {test_file} named {survivor_fn} for site {:?}",
                        entry.site
                    ));
                }
            }
            SurvivorProof::DataOnly { evidence } => {
                if evidence.trim().is_empty() {
                    errors.push(format!(
                        "allowlist entry {entry_name} has empty DataOnly evidence"
                    ));
                }
            }
        }
    }

    fail_with_errors("allowlist_entries_are_not_stale", errors);
}

#[test]
fn every_shared_helper_id_has_a_witness_in_each_calling_crate() {
    let result = scan();
    let helper_sites: BTreeMap<&str, &str> = HELPER_SITES.iter().copied().collect();
    let witnesses: BTreeSet<(&str, &str)> = SYNC_SITE_WITNESSES
        .iter()
        .map(|(site, crate_name, _, _)| (*site, *crate_name))
        .collect();
    let mut errors = Vec::new();

    for (crate_name, helper) in distinct_helper_calls(&result) {
        let Some(site) = helper_sites.get(helper).copied() else {
            errors.push(format!(
                "helper {helper:?} called by crate {crate_name:?} has no HELPER_SITES mapping"
            ));
            continue;
        };

        let has_witness = witnesses.contains(&(site, crate_name));
        let is_allowed = ALLOWLIST
            .iter()
            .any(|entry| entry.site == site && entry.crate_scope == Some(crate_name));

        if !has_witness && !is_allowed {
            errors.push(format!(
                "helper {helper:?} maps to site {site:?} in calling crate {crate_name:?}, \
                 but that pair has neither a witness nor an applicable allowlist entry"
            ));
        }
    }

    fail_with_errors(
        "every_shared_helper_id_has_a_witness_in_each_calling_crate",
        errors,
    );
}

#[test]
fn every_powerloss_suite_has_file_and_dir_controls_or_a_reasoned_exemption() {
    let result = scan();
    let exemptions: BTreeMap<&str, &str> = CONTROL_EXEMPTIONS.iter().copied().collect();
    let suites: BTreeSet<PathBuf> = result
        .test_fns
        .keys()
        .filter(|path| is_powerloss_suite(path))
        .cloned()
        .collect();
    let mut errors = Vec::new();

    for suite in suites {
        let suite_name = suite.to_string_lossy();
        if let Some(reason) = exemptions.get(suite_name.as_ref()) {
            if reason.trim().is_empty() {
                errors.push(format!(
                    "control exemption for {} has an empty reason",
                    suite.display()
                ));
            }
            continue;
        }

        let controls: BTreeSet<&str> = result
            .macros
            .iter()
            .filter(|macro_use| macro_use.kind == "crashsim_control" && macro_use.file == suite)
            .filter_map(|macro_use| macro_use.skip.as_deref())
            .collect();

        if !controls.contains("File") {
            errors.push(format!(
                "{} has no crashsim_control! with skip = File",
                suite.display()
            ));
        }
        if !controls.contains("Directory") {
            errors.push(format!(
                "{} has no crashsim_control! with skip = Directory",
                suite.display()
            ));
        }
    }

    fail_with_errors(
        "every_powerloss_suite_has_file_and_dir_controls_or_a_reasoned_exemption",
        errors,
    );
}

#[test]
fn every_witness_row_names_an_existing_macro_and_test() {
    let result = scan();
    let tracked_witnesses: BTreeSet<(&str, &str, &str)> = SYNC_SITE_WITNESSES
        .iter()
        .map(|(site, _, test_file, witness_fn)| (*test_file, *witness_fn, *site))
        .chain(
            UNATTRIBUTED_WITNESSES
                .iter()
                .map(|(site, test_file, witness_fn, _)| (*test_file, *witness_fn, *site)),
        )
        .collect();
    let helper_sites: BTreeMap<&str, &str> = HELPER_SITES.iter().copied().collect();
    let helper_calls = distinct_helper_calls(&result);
    let shared_sites: BTreeSet<&str> = HELPER_SITES.iter().map(|(_, site)| *site).collect();
    let mut errors = Vec::new();

    for (site, crate_name, test_file, witness_fn) in SYNC_SITE_WITNESSES {
        let matching = matching_macro(
            &result.macros,
            "crashsim_witness",
            test_file,
            witness_fn,
            Some(site),
        );

        match matching {
            None => errors.push(format!(
                "witness row ({site:?}, {crate_name:?}, {test_file:?}, {witness_fn:?}) \
                 has no matching crashsim_witness!"
            )),
            Some(macro_use) if macro_use.crate_name != *crate_name => errors.push(format!(
                "witness row for {witness_fn} expects crate {crate_name:?}, but its macro is in {:?}",
                macro_use.crate_name
            )),
            Some(_) => {}
        }

        if shared_sites.contains(site) {
            let calls_mapped_helper = helper_sites.iter().any(|(helper, mapped_site)| {
                mapped_site == site && helper_calls.contains(&(*crate_name, *helper))
            });
            let calls_atomic_publish = *site == "write_new_tmp_file:sync"
                && helper_calls.contains(&(*crate_name, "atomic_publish"));

            if !calls_mapped_helper && !calls_atomic_publish {
                errors.push(format!(
                    "witness row ({site:?}, {crate_name:?}, {test_file:?}, {witness_fn:?}) \
                     names a shared-helper site not called by that crate"
                ));
            }
        }
    }

    for (site, test_file, witness_fn, reason) in UNATTRIBUTED_WITNESSES {
        if reason.trim().is_empty() {
            errors.push(format!(
                "unattributed witness row ({site:?}, {test_file:?}, {witness_fn:?}) has an empty reason"
            ));
        }

        if matching_macro(
            &result.macros,
            "crashsim_witness",
            test_file,
            witness_fn,
            Some(site),
        )
        .is_none()
        {
            errors.push(format!(
                "unattributed witness row ({site:?}, {test_file:?}, {witness_fn:?}) \
                 has no matching crashsim_witness!"
            ));
        }
    }

    for macro_use in &result.macros {
        let tests = result.test_fns.get(&macro_use.file);
        if !tests.is_some_and(|tests| tests.contains(&macro_use.body)) {
            errors.push(format!(
                "{}! {} in {} names body {:?}, which is not a #[test] fn in that file",
                macro_use.kind,
                macro_use.fn_name,
                macro_use.file.display(),
                macro_use.body
            ));
        }

        if macro_use.kind == "crashsim_witness" && macro_use.crate_name != "htap-crashsim" {
            match macro_use.site.as_deref() {
                Some(site)
                    if tracked_witnesses.contains(&(
                        macro_use.file.to_string_lossy().as_ref(),
                        macro_use.fn_name.as_str(),
                        site,
                    )) => {}
                Some(site) => errors.push(format!(
                    "untracked crashsim_witness! {} in {} for site {site:?}",
                    macro_use.fn_name,
                    macro_use.file.display()
                )),
                None => errors.push(format!(
                    "crashsim_witness! {} in {} has no site",
                    macro_use.fn_name,
                    macro_use.file.display()
                )),
            }
        }
    }

    fail_with_errors("every_witness_row_names_an_existing_macro_and_test", errors);
}

#[test]
fn every_production_site_id_is_used_at_one_location() {
    let result = scan();
    let mut locations = BTreeMap::<&str, BTreeSet<(&Path, usize)>>::new();

    for site_use in &result.sites {
        locations
            .entry(site_use.site.as_str())
            .or_default()
            .insert((site_use.file.as_path(), site_use.line));
    }

    let errors = locations
        .into_iter()
        .filter(|(_, locations)| locations.len() != 1)
        .map(|(site, locations)| {
            let locations = locations
                .into_iter()
                .map(|(file, line)| format!("{}:{line}", file.display()))
                .collect::<Vec<_>>()
                .join(", ");
            format!("production site id {site:?} is used at multiple locations: {locations}")
        })
        .collect();

    fail_with_errors("every_production_site_id_is_used_at_one_location", errors);
}

#[test]
fn scanner_observes_the_workspace() {
    let result = scan();
    let site_count = observed_sites(&result).len();
    let helper_call_count = distinct_helper_calls(&result).len();
    let witness_count = result
        .macros
        .iter()
        .filter(|macro_use| macro_use.kind == "crashsim_witness")
        .count();
    let control_file_count = result
        .macros
        .iter()
        .filter(|macro_use| macro_use.kind == "crashsim_control")
        .map(|macro_use| &macro_use.file)
        .collect::<BTreeSet<_>>()
        .len();

    assert!(
        site_count >= 30,
        "scanner observed {site_count} distinct production site ids; expected at least 30"
    );
    assert!(
        helper_call_count >= 5,
        "scanner observed {helper_call_count} distinct (crate, helper) calls; expected at least 5"
    );
    assert!(
        witness_count >= 30,
        "scanner observed {witness_count} crashsim_witness! macros; expected at least 30"
    );
    assert!(
        control_file_count >= 15,
        "scanner observed crashsim_control! macros in {control_file_count} distinct files; expected at least 15"
    );
}

#[test]
fn no_production_sync_is_untagged() {
    let result = scan();
    let errors = result
        .violations
        .iter()
        .map(|violation| {
            format!(
                "{}:{} {} {}",
                violation.file.display(),
                violation.line,
                violation.fn_name,
                violation.reason
            )
        })
        .collect();

    fail_with_errors("no_production_sync_is_untagged", errors);
}

#[test]
fn every_sync_site_is_listed_in_the_architecture_catalog() {
    let result = scan();
    let observed: BTreeSet<String> = observed_sites(&result)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let architecture_path = mutation_controls_scan::workspace_root().join("docs/ARCHITECTURE.md");
    let architecture = std::fs::read_to_string(&architecture_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", architecture_path.display()));
    let lines: Vec<&str> = architecture.lines().collect();
    let mut errors = Vec::new();
    let mut catalog = BTreeSet::new();
    let mut duplicates = BTreeSet::new();

    let heading_index = lines.iter().position(|line| {
        let trimmed = line.trim_start();
        let hashes = trimmed
            .chars()
            .take_while(|character| *character == '#')
            .count();
        hashes > 0
            && trimmed
                .get(hashes..)
                .is_some_and(|text| text.trim().contains("Durability-point catalog"))
    });

    match heading_index {
        None => errors.push(format!(
            "{} has no heading containing \"Durability-point catalog\"",
            architecture_path.display()
        )),
        Some(heading_index) => {
            let table_start = lines
                .iter()
                .enumerate()
                .skip(heading_index + 1)
                .take_while(|(_, line)| !line.trim_start().starts_with('#'))
                .find(|(_, line)| line.contains('|'))
                .map(|(index, _)| index);

            match table_start {
                None => errors.push(format!(
                    "{} has no markdown table following the durability-point catalog heading",
                    architecture_path.display()
                )),
                Some(table_start) => {
                    let table_rows: Vec<&str> = lines
                        .iter()
                        .skip(table_start)
                        .take_while(|line| {
                            let trimmed = line.trim_start();
                            !trimmed.starts_with('#') && line.contains('|')
                        })
                        .copied()
                        .collect();

                    if table_rows.len() < 2 {
                        errors.push(format!(
                            "durability-point catalog in {} has no header and separator rows",
                            architecture_path.display()
                        ));
                    } else {
                        for (offset, row) in table_rows.iter().enumerate().skip(2) {
                            let first_cell = row
                                .trim()
                                .trim_start_matches('|')
                                .split('|')
                                .next()
                                .unwrap_or_default()
                                .trim();
                            let Some(site) = first_cell
                                .strip_prefix('`')
                                .and_then(|cell| cell.strip_suffix('`'))
                                .filter(|site| !site.is_empty() && !site.contains('`'))
                            else {
                                errors.push(format!(
                                    "durability-point catalog row {} has no backticked id in its first cell",
                                    table_start + offset + 1
                                ));
                                continue;
                            };

                            if !catalog.insert(site.to_owned()) {
                                duplicates.insert(site.to_owned());
                            }
                        }
                    }
                }
            }
        }
    }

    if observed.is_empty() {
        errors.push("scanner observed no production sync-site ids".to_owned());
    }
    if catalog.is_empty() {
        errors.push("durability-point catalog contains no production sync-site ids".to_owned());
    }

    for site in observed.difference(&catalog) {
        errors.push(format!(
            "production sync-site id {site:?} is missing from the durability-point catalog"
        ));
    }
    for site in catalog.difference(&observed) {
        errors.push(format!(
            "durability-point catalog contains stale sync-site id {site:?}"
        ));
    }
    for site in duplicates {
        errors.push(format!(
            "durability-point catalog contains duplicate rows for sync-site id {site:?}"
        ));
    }

    fail_with_errors(
        "every_sync_site_is_listed_in_the_architecture_catalog",
        errors,
    );
}
