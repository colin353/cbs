mod actions;
#[cfg(test)]
mod bus;
#[cfg(test)]
mod cargo;
#[cfg(test)]
mod cargo_recipes;
mod context;
mod core;
mod exec;
mod plugin_abi;
mod plugins;
#[cfg(test)]
mod rust_plugin;
mod workspace;

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn run() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        return Err(usage_error());
    };
    match command.as_str() {
        "build" => {
            let targets: Vec<String> = args.collect();
            if targets.is_empty() {
                return Err(usage_error());
            };
            eprintln!("[cbs] build requested: {}", targets.join(", "));
            match workspace::build_from_current_workspace(&targets)? {
                core::BuildResult::Success(output) => {
                    for output in output.outputs {
                        println!("{}", output.display());
                    }
                    Ok(())
                }
                core::BuildResult::Failure(reason) => Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("build failed: {reason}"),
                )),
            }
        }
        "test" => {
            let targets: Vec<String> = args.collect();
            if targets.is_empty() {
                return Err(usage_error());
            };
            eprintln!("[cbs] test requested: {}", targets.join(", "));
            let build = workspace::build_tests_from_current_workspace(&targets)?;
            match build.result {
                core::BuildResult::Success(output) => run_tests(build.targets, output.outputs),
                core::BuildResult::Failure(reason) => Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("build failed: {reason}"),
                )),
            }
        }
        "run" => {
            let Some(target) = args.next() else {
                return Err(usage_error());
            };
            let run_args: Vec<String> = match args.next().as_deref() {
                Some("--") => args.collect(),
                Some(arg) => std::iter::once(arg.to_string()).chain(args).collect(),
                None => Vec::new(),
            };
            eprintln!("[cbs] run requested: {target}");
            match workspace::build_from_current_workspace(std::slice::from_ref(&target))? {
                core::BuildResult::Success(output) => {
                    let executable = output.outputs.first().ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("{target} did not produce an executable output"),
                        )
                    })?;
                    eprintln!("[cbs] execute {}", executable.display());
                    let status = std::process::Command::new(executable)
                        .args(run_args)
                        .status()?;
                    if status.success() {
                        Ok(())
                    } else {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("{target} exited with {status}"),
                        ))
                    }
                }
                core::BuildResult::Failure(reason) => Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("build failed: {reason}"),
                )),
            }
        }
        _ => Err(usage_error()),
    }
}

fn run_tests(targets: Vec<String>, executables: Vec<std::path::PathBuf>) -> std::io::Result<()> {
    if targets.len() != executables.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "expected one executable per test target, got {} target(s) and {} output(s)",
                targets.len(),
                executables.len()
            ),
        ));
    }

    let mut failed = Vec::new();
    for (target, executable) in targets.iter().zip(executables.iter()) {
        eprintln!("[cbs] test {target}");
        let output = match std::process::Command::new(executable).output() {
            Ok(output) => output,
            Err(e) => {
                eprintln!("[cbs] test FAIL {target}");
                eprintln!("--- {target} execution error ---");
                eprintln!("failed to execute {}: {e}", executable.display());
                failed.push(target.clone());
                continue;
            }
        };
        if output.status.success() {
            eprintln!("[cbs] test PASS {target}");
        } else {
            eprintln!("[cbs] test FAIL {target}");
            print_test_failure_logs(target, &output);
            failed.push(target.clone());
        }
    }

    if failed.is_empty() {
        eprintln!("[cbs] test result: {} passed", targets.len());
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("{} test(s) failed: {}", failed.len(), failed.join(", ")),
        ))
    }
}

fn print_test_failure_logs(target: &str, output: &std::process::Output) {
    eprintln!("--- {target} status ---");
    eprintln!("{}", output.status);
    eprintln!("--- {target} stdout ---");
    if output.stdout.is_empty() {
        eprintln!("<empty>");
    } else {
        eprint!("{}", String::from_utf8_lossy(&output.stdout));
        if !output.stdout.ends_with(b"\n") {
            eprintln!();
        }
    }
    eprintln!("--- {target} stderr ---");
    if output.stderr.is_empty() {
        eprintln!("<empty>");
    } else {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        if !output.stderr.ends_with(b"\n") {
            eprintln!();
        }
    }
}

fn usage_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "usage: cbs build <target-or-pattern>...\n       cbs test <target-or-pattern>...\n       cbs run //package:target | :target [-- args...]",
    )
}
