//! lmsql: one SQL statement in, structured JSON out. A native lmapi tool (`lmsql --schema`)
//! built on lmrust; SQLite in phase 1.

pub mod backend;
pub mod connection;
pub mod error;
pub mod legacy;
pub mod spec;
pub mod values;

/// Run with the arguments after the binary name; returns the exit code.
///
/// lmapi-shaped argv (a published op such as `query`, or a root lmapi global such as `--schema`)
/// goes to the lmapi runner: that is also the human CLI (`lmsql query --db x.sqlite "select 1"`).
/// Everything else (no arguments, `--help`, `--version`, unknown words) is the small legacy
/// surface in [`legacy`].
pub fn run(args: Vec<String>) -> i32 {
  let spec = spec::spec();
  if let Some(code) = lmrust::server::maybe_run_cli(&spec, args.clone()) {
    return code;
  }
  legacy::run(&args, &mut std::io::stdout(), &mut std::io::stderr())
}
