//! Shows the NavBridge splash on the panel while the Pi boots: from when the
//! display driver starts until the UI takes the display. Started by udev as
//! soon as `/dev/fb0` appears (scripts/install_splash.sh installs it).

mod framebuffer;
mod picture;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use framebuffer::{release_console, Framebuffer};
use picture::Picture;

const USAGE: &str = "\
Usage: navbridge-splash [--framebuffer PATH] [--keep-console] IMAGE.png
       navbridge-splash --check IMAGE.png

Shows IMAGE.png on the framebuffer (default /dev/fb0), stretched to the
panel's size, and stops the text console drawing over it.
--keep-console  leave the text console as it is.
--check         only read the image and print its size.";

struct Options {
    framebuffer: PathBuf,
    keep_console: bool,
    check: bool,
    image: PathBuf,
}

fn options(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut framebuffer = PathBuf::from("/dev/fb0");
    let (mut keep_console, mut check, mut image) = (false, false, None);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--framebuffer" => {
                framebuffer = args.next().ok_or("--framebuffer needs a path")?.into();
            }
            "--keep-console" => keep_console = true,
            "--check" => check = true,
            "--help" | "-h" => return Err(String::new()),
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}")),
            _ if image.is_none() => image = Some(PathBuf::from(arg)),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    Ok(Options {
        framebuffer,
        keep_console,
        check,
        image: image.ok_or("no image given")?,
    })
}

fn run(options: &Options) -> Result<(), String> {
    let bytes = std::fs::read(&options.image)
        .map_err(|e| format!("read {}: {e}", options.image.display()))?;
    let picture = Picture::decode(&bytes)?;
    if options.check {
        println!(
            "{}: {}x{}",
            options.image.display(),
            picture.width,
            picture.height
        );
        return Ok(());
    }
    if !options.keep_console {
        // The splash still shows if this fails; the console may then draw on it.
        if let Err(e) = release_console(Path::new("/sys/class/vtconsole")) {
            eprintln!("navbridge-splash: release the text console: {e}");
        }
    }
    let framebuffer = Framebuffer::open(&options.framebuffer)?;
    framebuffer.show(&picture)?;
    let layout = &framebuffer.layout;
    println!(
        "Splash shown at {}x{}, {} bits per pixel",
        layout.width,
        layout.height,
        layout.bytes_per_pixel * 8
    );
    Ok(())
}

fn main() -> ExitCode {
    let options = match options(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(e) if e.is_empty() => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("navbridge-splash: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("navbridge-splash: {e}");
            ExitCode::FAILURE
        }
    }
}
