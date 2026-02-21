use clap::Parser;

#[derive(Parser)]
#[command(name = "olsvr", about = "OLED screensaver for Wayland")]
struct Args {
    /// Idle timeout in minutes before activation
    #[arg(long, default_value_t = 5)]
    timeout: u32,

    /// Immediately activate the screensaver
    #[arg(long)]
    activate: bool,

    /// Clock font size in pixels
    #[arg(long, default_value_t = 200)]
    font_size: u32,

    /// Hold duration between fades in seconds
    #[arg(long, default_value_t = 10)]
    hold: u32,
}

fn main() {
    env_logger::init();
    let args = Args::parse();
    println!(
        "olsvr: timeout={}m, font_size={}px, hold={}s",
        args.timeout, args.font_size, args.hold
    );
}
