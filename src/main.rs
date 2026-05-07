mod actions;
mod cargo;
mod cargo_recipes;
mod context;
mod core;
mod exec;
mod plugins;
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
            let Some(target) = args.next() else {
                return Err(usage_error());
            };
            if args.next().is_some() {
                return Err(usage_error());
            }
            eprintln!("[cbs] build requested: {target}");
            match workspace::build_from_current_workspace(&target)? {
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
            match workspace::build_from_current_workspace(&target)? {
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

fn usage_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "usage: cbs build //package:target | :target\n       cbs run //package:target | :target [-- args...]",
    )
}
