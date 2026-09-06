fn main() {
    std::process::exit(pipeline::entrypoint(std::env::args_os().collect()));
}
