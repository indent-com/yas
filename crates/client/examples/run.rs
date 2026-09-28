//! Run a command on a YAS server and print its output.
//!
//! ```sh
//! cargo run -p yas-client --example run -- local -- uname -a
//! cargo run -p yas-client --example run -- ssh:me@host -- ls /
//! ```
//!
//! The first argument is any target the `yas` CLI accepts. `local` needs a
//! running `yas server` (this example never starts one).

use yas_client::process::Command;
use yas_client::{Client, ConnectOptions};

#[tokio::main]
async fn main() -> yas_client::Result<()> {
    let mut args = std::env::args().skip(1);
    let target = args.next().unwrap_or_else(|| "local".into());
    let argv: Vec<String> = args.skip_while(|arg| arg == "--").collect();
    if argv.is_empty() {
        eprintln!("usage: run TARGET -- PROGRAM [ARGS...]");
        std::process::exit(2);
    }

    let client =
        Client::connect(Some(&target), &ConnectOptions::named("yas-client-example")).await?;
    eprintln!(
        "connected to {} (YAS {})",
        client.server_name(),
        client.hello().server_release
    );

    let output = client
        .spawn(Command::new(&argv[0]).args(&argv[1..]))
        .await?
        .output()
        .await?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    eprintln!("{}", output.status);
    std::process::exit(output.status.code().unwrap_or(1));
}
