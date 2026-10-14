use std::{
    fs::File,
    io::BufWriter,
    time::{Duration, Instant},
};
use tail_window_access::desktop::{
    Button, Desktop, InputAuthority, KeyCode, Position, Transition, observe_lock,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("desktop-probe failed: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--help" {
        println!(
            "desktop-probe capabilities [output]\ndesktop-probe snapshot <new-png-path> [output]\ndesktop-probe input-selftest --allow-input <normalized-x> <normalized-y> [output]\ndesktop-probe cleanup-selftest --allow-input <normalized-x> <normalized-y> [output]\ndesktop-probe clipboard-selftest --allow-clipboard-write [output]\n\nDefault output is eDP-1. Input selftest clicks, types twa, and scrolls. Cleanup selftest drops a device with Shift and the left button held. Clipboard selftest replaces the clipboard with a fixed test value. No network listener is created."
        );
        return Ok(());
    }
    match args[0].as_str() {
        "capabilities" if args.len() <= 2 => {
            let desktop = Desktop::connect(args.get(1).map_or("eDP-1", String::as_str))?;
            println!("{:#?}", desktop.capabilities());
        }
        "snapshot" if (2..=3).contains(&args.len()) => {
            let mut desktop = Desktop::connect(args.get(2).map_or("eDP-1", String::as_str))?;
            let frame = desktop.capture(Duration::from_secs(3))?;
            let file = File::create_new(&args[1])?;
            let mut encoder = png::Encoder::new(BufWriter::new(file), frame.width, frame.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header()?;
            writer.write_image_data(&frame.rgba()?)?;
            writer.finish()?;
            println!(
                "captured output={} width={} height={} stride={} format={:?} generation={} scale={} transform={} y_inverted={}",
                frame.geometry.output,
                frame.width,
                frame.height,
                frame.stride,
                frame.format,
                frame.geometry.generation,
                frame.geometry.scale,
                frame.geometry.transform,
                frame.y_inverted
            );
        }
        "input-selftest" | "cleanup-selftest"
            if (4..=5).contains(&args.len()) && args[1] == "--allow-input" =>
        {
            let position = Position::new(args[2].parse()?, args[3].parse()?)?;
            let mut desktop = Desktop::connect(args.get(4).map_or("eDP-1", String::as_str))?;
            let geometry = desktop.refresh()?;
            let authority = InputAuthority::default();
            authority.observe_lock(observe_lock(Duration::from_millis(150)));
            if !authority.is_open() {
                return Err("input requires a current verified lock state".into());
            }
            let permit =
                || authority.permit(geometry.generation, Instant::now() + Duration::from_secs(3));
            desktop.button(position, Button::Left, Transition::Press, permit())?;
            if args[0] == "cleanup-selftest" {
                desktop.key(KeyCode::new(42)?, Transition::Press, permit())?;
                drop(desktop);
                println!("virtual device dropped; verify held-input cleanup in the fixture");
            } else {
                desktop.button(position, Button::Left, Transition::Release, permit())?;
                for code in [20, 17, 30] {
                    let key = KeyCode::new(code)?;
                    desktop.key(key, Transition::Press, permit())?;
                    desktop.key(key, Transition::Release, permit())?;
                }
                desktop.axis(position, 0.0, 15.0, permit())?;
                desktop.release_all()?;
                println!("input sequence completed and virtual inputs released");
            }
        }
        "clipboard-selftest"
            if (2..=3).contains(&args.len()) && args[1] == "--allow-clipboard-write" =>
        {
            let mut desktop = Desktop::connect(args.get(2).map_or("eDP-1", String::as_str))?;
            let expected = "TailWindowAccess clipboard probe\nUTF-8 café";
            desktop.clipboard_write(expected)?;
            if desktop.clipboard_read()? != expected {
                return Err("clipboard roundtrip did not match".into());
            }
            println!("bounded UTF-8 clipboard roundtrip passed");
        }
        _ => return Err("invalid arguments; run desktop-probe --help".into()),
    }
    Ok(())
}
