use clap::{Command, CommandFactory};
use ullage_cli::Cli;

#[test]
fn every_command_and_argument_has_about() {
    let command = Cli::command();
    let mut missing = Vec::new();
    collect_missing_about(&command, "ullage", &mut missing);
    assert!(
        missing.is_empty(),
        "CLI help is missing descriptions:\n{}",
        missing.join("\n")
    );
}

#[test]
fn help_covers_exit_codes_examples_and_value_names() {
    let command = Cli::command();
    let long_about = command
        .get_long_about()
        .map(ToString::to_string)
        .unwrap_or_default();
    for fragment in [
        "0   success",
        "1   failure",
        "2   partial",
        "3   authentication invalid",
        "4   network failure",
        "5   daemon unavailable",
        "6   protocol error",
        "64  usage",
    ] {
        assert!(
            long_about.contains(fragment),
            "top-level long_about missing {fragment:?}: {long_about}"
        );
    }

    assert_after_help(&command, &[], "ullage daemon install");
    assert_after_help(&command, &[], "ullage device pair");
    assert_after_help(
        &command,
        &["auth", "login"],
        "ullage --reveal auth login claude --account claude-work",
    );
    assert_after_help(&command, &["probe"], "ullage probe claude-work");
    assert_after_help(&command, &["show"], "ullage show --all");
    assert_after_help(&command, &["device"], "ullage device revoke <DEVICE_ID>");
    assert_after_help(&command, &["daemon", "install"], "ullage daemon start");

    let login = find_command(&command, &["auth", "login"]);
    let method = login
        .get_arguments()
        .find(|arg| arg.get_id() == "method")
        .expect("auth login --method");
    let method_help = format!(
        "{} {}",
        text(method.get_help()),
        method
            .get_possible_values()
            .iter()
            .filter_map(|value| value.get_help().map(ToString::to_string))
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert!(
        method_help.contains("auth complete"),
        "device-code help should tell scripts to repeat auth complete: {method_help}"
    );

    let probe_long = text(find_command(&command, &["probe"]).get_long_about());
    assert!(
        probe_long.contains("--no-wait"),
        "probe long_about should explain --no-wait: {probe_long}"
    );
    let show_long = text(find_command(&command, &["show"]).get_long_about());
    assert!(
        show_long.contains("--all"),
        "show long_about should explain --all: {show_long}"
    );
    for flag in ["--metric", "--no-metric-filter"] {
        assert!(
            show_long.contains(flag),
            "show long_about should explain {flag}: {show_long}"
        );
    }

    assert_value_name(&command, &["account", "add"], "provider", "PROVIDER_ID");
    assert_value_name(&command, &["account", "show"], "account", "ACCOUNT_ID");
    assert_value_name(&command, &["account", "metrics"], "account", "ACCOUNT_ID");
    assert_value_name(&command, &["account", "metrics"], "metrics", "METRIC");
    assert_value_name(&command, &["account", "label"], "label", "ACCOUNT_LABEL");
    assert_value_name(&command, &["auth", "login"], "provider", "PROVIDER_ID");
    assert_value_name(&command, &["auth", "complete"], "flow_id", "FLOW_ID");
    assert_value_name(
        &command,
        &["auth", "complete"],
        "authorization_code_env",
        "ENV_VAR",
    );
    assert_value_name(&command, &["probe"], "account", "ACCOUNT_ID");
    assert_value_name(&command, &["show"], "account", "ACCOUNT_ID");
    assert_value_name(&command, &["show"], "metric", "METRIC");
    assert_value_name(&command, &["device", "revoke"], "device_id", "DEVICE_ID");

    assert!(command.find_subcommand("http").is_none());
    assert!(command.find_subcommand("workspace").is_none());
}

fn collect_missing_about(command: &Command, path: &str, missing: &mut Vec<String>) {
    if text(command.get_about()).is_empty() {
        missing.push(format!("command {path} is missing about"));
    }
    for arg in command.get_arguments() {
        let id = arg.get_id().as_str();
        if arg.is_hide_set() || matches!(id, "help" | "version") {
            continue;
        }
        if text(arg.get_help()).is_empty() {
            missing.push(format!("argument {path} {id} is missing about"));
        }
    }
    for subcommand in command.get_subcommands() {
        let child = format!("{path} {}", subcommand.get_name());
        collect_missing_about(subcommand, &child, missing);
    }
}

fn assert_after_help(root: &Command, path: &[&str], expected: &str) {
    let command = find_command(root, path);
    let after_help = text(command.get_after_help());
    assert!(
        after_help.contains(expected),
        "after_help for {} is missing {expected:?}: {after_help}",
        display_path(path)
    );
}

fn assert_value_name(root: &Command, path: &[&str], arg_id: &str, expected: &str) {
    let command = find_command(root, path);
    let names = command
        .get_arguments()
        .find(|arg| arg.get_id() == arg_id)
        .and_then(|arg| arg.get_value_names())
        .map(|names| {
            names
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    assert_eq!(
        names,
        expected,
        "value_name for {} {arg_id}",
        display_path(path)
    );
}

fn find_command<'a>(root: &'a Command, path: &[&str]) -> &'a Command {
    let mut command = root;
    for name in path {
        command = command
            .find_subcommand(name)
            .unwrap_or_else(|| panic!("missing subcommand {}", display_path(path)));
    }
    command
}

fn display_path(path: &[&str]) -> String {
    if path.is_empty() {
        "ullage".into()
    } else {
        format!("ullage {}", path.join(" "))
    }
}

fn text(value: Option<&impl ToString>) -> String {
    value
        .map(ToString::to_string)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

#[test]
fn raw_is_global_and_documented_as_table_only() {
    let mut command = Cli::command();
    command.build();
    for path in [&[][..], &["show"][..], &["probe"][..]] {
        let raw = find_command(&command, path)
            .get_arguments()
            .find(|argument| argument.get_id() == "raw")
            .unwrap_or_else(|| panic!("--raw is missing from {path:?}"))
            .clone();
        assert!(raw.is_global_set(), "--raw must be a global flag");
        let help = format!("{} {}", text(raw.get_help()), text(raw.get_long_help()));
        assert!(help.contains("Table output only"), "{help}");
        assert!(help.contains("--output json"), "{help}");
    }
}
