fn main() {
    match flashgrep::run_cli() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("flashgrep[startup][error] {error}");
            std::process::exit(2);
        }
    }
}
