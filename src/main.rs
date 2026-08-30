//! Goal: start the process while keeping all testable behavior in the
//! library crate.

fn main() {
    let config = match infernal_worker_simple::Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("configuration error: {error}");
            std::process::exit(1);
        }
    };
    infernal_worker_simple::run(config);
}
