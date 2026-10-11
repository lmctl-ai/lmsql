fn main() {
  let mut args = Vec::new();
  for arg in std::env::args_os().skip(1) {
    match arg.into_string() {
      Ok(a) => args.push(a),
      Err(raw) => {
        eprintln!("lmsql: arguments must be valid UTF-8 (got {raw:?})");
        std::process::exit(2);
      }
    }
  }
  std::process::exit(lmsql::run(args));
}
