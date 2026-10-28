//! CI workflow timeout and per-package test invariants.

use std::fs;
use std::path::PathBuf;

const PACKAGE_SCRIPT: &str = ".github/scripts/test-each-package.sh";

#[derive(Default)]
struct Job {
    name: String,
    has_timeout: bool,
    runs_package_script: bool,
    steps: Vec<Step>,
}

#[derive(Default)]
struct Step {
    is_cargo_cache: bool,
    has_timeout: bool,
}

#[test]
fn ci_workflow_enforces_timeouts_and_per_package_tests() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one directory below the workspace root")
        .to_path_buf();
    let workflow =
        fs::read_to_string(root.join(".github/workflows/ci.yml")).expect("read CI workflow");
    let violations = workflow_violations(&workflow);
    assert!(
        violations.is_empty(),
        "CI workflow violations:\n{}",
        violations.join("\n")
    );

    let script = fs::read_to_string(root.join(PACKAGE_SCRIPT)).expect("read package test script");
    assert!(script.contains("cargo"), "script must invoke Cargo");
    assert!(script.contains(" test "), "script must run Cargo tests");
    assert!(script.contains(" -p "), "script must select each package");
    assert!(
        !script.contains("--all-features") && !script.contains("--features"),
        "per-package tests must use only default features"
    );
}

#[test]
fn workflow_parser_reports_missing_job_and_cache_timeouts() {
    let workflow = r#"jobs:
  broken:
    runs-on: ubuntu-latest
    steps:
      - name: Cache Cargo
        uses: Swatinem/rust-cache@v2
      - name: Test each package
        run: bash .github/scripts/test-each-package.sh
"#;
    assert_eq!(
        workflow_violations(workflow),
        [
            "job broken has no timeout-minutes",
            "job broken cache step 1 has no timeout-minutes",
        ]
    );
}

fn workflow_violations(workflow: &str) -> Vec<String> {
    let mut jobs = Vec::<Job>::new();
    let mut in_jobs = false;
    for line in workflow.lines() {
        let trimmed_start = line.trim_start();
        let indentation = line.len() - trimmed_start.len();
        let trimmed = trimmed_start.trim_end();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if indentation == 0 {
            in_jobs = trimmed == "jobs:";
            continue;
        }
        if !in_jobs {
            continue;
        }
        if indentation == 2 && trimmed.ends_with(':') {
            jobs.push(Job {
                name: trimmed.trim_end_matches(':').to_owned(),
                ..Job::default()
            });
            continue;
        }
        let Some(job) = jobs.last_mut() else {
            continue;
        };
        match indentation {
            4 if trimmed.starts_with("timeout-minutes:") => job.has_timeout = true,
            6 if trimmed.starts_with("- ") => job.steps.push(Step::default()),
            8 => {
                if trimmed.starts_with("timeout-minutes:") {
                    if let Some(step) = job.steps.last_mut() {
                        step.has_timeout = true;
                    }
                } else if trimmed.starts_with("uses: Swatinem/rust-cache") {
                    if let Some(step) = job.steps.last_mut() {
                        step.is_cargo_cache = true;
                    }
                } else if trimmed.starts_with("run:") && trimmed.contains(PACKAGE_SCRIPT) {
                    job.runs_package_script = true;
                }
            }
            _ => {}
        }
    }

    let mut violations = Vec::new();
    for job in &jobs {
        if !job.has_timeout {
            violations.push(format!("job {} has no timeout-minutes", job.name));
        }
        for (index, step) in job.steps.iter().enumerate() {
            if step.is_cargo_cache && !step.has_timeout {
                violations.push(format!(
                    "job {} cache step {} has no timeout-minutes",
                    job.name,
                    index + 1
                ));
            }
        }
    }
    if !jobs.iter().any(|job| job.runs_package_script) {
        violations.push(format!("no job runs {PACKAGE_SCRIPT}"));
    }
    violations
}
