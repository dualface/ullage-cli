use std::io::Write;

use ullage_cli::{ExitCode, RunOutput, run_from};

#[tokio::main]
async fn main() {
    ullage_daemon::install_redacting_panic_hook();
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__daemon")) {
        if let Err(error) = ullage_app::run_daemon().await {
            eprintln!("ullage daemon failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    let client = ullage_app::ProductionClient::from_environment();
    let output = run_from(std::env::args_os(), &client);
    std::process::exit(print_output(output));
}

fn print_output(output: RunOutput) -> i32 {
    let stdout_result = std::io::stdout().write_all(output.stdout.as_bytes());
    let stderr_result = std::io::stderr().write_all(output.stderr.as_bytes());
    if stdout_result.is_err() || stderr_result.is_err() {
        ExitCode::Failure as i32
    } else {
        output.code as i32
    }
}
