use std::io::Write;

use ullage_cli::{ExitCode, RunOutput, run_from};

fn main() {
    ullage_daemon::install_redacting_panic_hook();
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    // Only the hidden `__daemon` entrypoint needs a Tokio runtime; every
    // ordinary CLI path stays synchronous so it never pays for one.
    if arguments.next().as_deref() == Some(std::ffi::OsStr::new("__daemon")) {
        if arguments.next().is_some() {
            eprintln!("ullage daemon failed: unexpected arguments after __daemon");
            std::process::exit(ExitCode::Usage as i32);
        }
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("ullage daemon failed: {error}");
                std::process::exit(ExitCode::Failure as i32);
            }
        };
        if let Err(error) = runtime.block_on(ullage_app::run_daemon()) {
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
