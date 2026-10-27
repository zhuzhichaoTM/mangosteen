//! `mangosteen` 二进制入口。

fn main() {
    let code = mangosteen_cli::run();
    std::process::exit(code);
}
