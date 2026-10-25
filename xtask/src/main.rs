//! Local maintenance tasks for the hofund workspace.
//!
//! Run `cargo xtask help` for the task list. This crate is not part of the gateway
//! runtime, and no workspace crate may depend on it. Architecture checks live in
//! this crate's integration tests.

use std::path::Path;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None | Some("help" | "--help" | "-h") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("check-plugins") => check_plugins(),
        Some(task) => {
            eprintln!("unknown xtask `{task}`");
            ExitCode::from(2)
        }
    }
}

fn print_help() {
    println!(
        "\
hofund xtask

Tasks:
  help            Show this help
  check-plugins   cargo check -p each plugin, so the kernel feature is not unified in

Dependency direction and the vendor-name rule are enforced by
`cargo test --workspace`."
    );
}

/// Package names under `plugins/<kind>/<id>/Cargo.toml`.
///
/// Kinds are the directories the seam allows: providers, subscriptions, agents,
/// and inbound. An empty or missing directory is an empty list.
fn plugin_packages(plugins: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let kinds = ["providers", "subscriptions", "agents", "inbound"];
    let Ok(entries) = std::fs::read_dir(plugins) else {
        return names;
    };
    for entry in entries.flatten() {
        if !kinds.contains(&entry.file_name().to_string_lossy().as_ref()) {
            continue;
        }
        let Ok(children) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for child in children.flatten() {
            let manifest = child.path().join("Cargo.toml");
            let Ok(text) = std::fs::read_to_string(&manifest) else {
                continue;
            };
            let Ok(value) = toml::from_str::<toml::Value>(&text) else {
                continue;
            };
            if let Some(name) = value
                .get("package")
                .and_then(|package| package.get("name"))
                .and_then(toml::Value::as_str)
            {
                names.push(name.to_owned());
            }
        }
    }
    names.sort();
    names
}

fn check_plugins() -> ExitCode {
    let plugins = Path::new("plugins");
    let packages = plugin_packages(plugins);
    if packages.is_empty() {
        println!("no plugin packages to check");
        return ExitCode::SUCCESS;
    }
    for package in &packages {
        println!("cargo check -p {package}");
        let status = Command::new("cargo")
            .args(["check", "-p", package])
            .status();
        match status {
            Ok(status) if status.success() => {}
            Ok(status) => {
                eprintln!("cargo check -p {package} failed ({status})");
                return ExitCode::from(1);
            }
            Err(error) => {
                eprintln!("could not run cargo check -p {package}: {error}");
                return ExitCode::from(1);
            }
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::plugin_packages;

    #[test]
    fn plugin_packages_reads_the_package_name() {
        let root =
            std::env::temp_dir().join(format!("hofund-plugin-packages-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let plugin = root.join("plugins/providers/example");
        std::fs::create_dir_all(&plugin).expect("dir");
        std::fs::write(
            plugin.join("Cargo.toml"),
            "[package]\nname = \"hofund-plugin-example\"\n",
        )
        .expect("manifest");
        let names = plugin_packages(&root.join("plugins"));
        assert_eq!(names, ["hofund-plugin-example".to_owned()]);
        let _ = std::fs::remove_dir_all(&root);
        assert!(plugin_packages(&root.join("missing")).is_empty());
    }
}
