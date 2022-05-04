//! Reads the latest file in the given directory whose file name starts with "vlcsnap-",
//! automatically crops borders of black pixels from the image, lightens the image by the specified
//! amount, and sends it to a default printer.
//! Currently, printing only works on Windows.

#![deny(missing_docs)]

use anyhow::{Context, Result};
use argh::FromArgs;
use image::io::Reader as ImageReader;
use image::{buffer::Pixels, imageops, ImageBuffer, Pixel, SubImage};
use std::fs::{self, DirEntry};
use std::path::{PathBuf, Path};
use std::time::SystemTime;
use pbr::ProgressBar;
use std::process::Command;
use anyhow::bail;

#[derive(FromArgs)]
/// Reads the latest file in the given directory whose file name starts with "vlcsnap-",
/// automatically crops borders of black pixels from the image, lightens the image by the specified
/// amount, and sends it to a default printer.
/// Currently only works on Windows.
struct Args {
    /// which directory to look for snapshots in; you probably want this to be the same directory
    /// that VLC is set to save snapshots to.
    #[argh(option, short = 'd')]
    snapshot_dir: PathBuf,

    /// where to map zero in the squooshed 0..u16::MAX range
    #[argh(option, short = 'l')]
    luma: u8,
}

/// Guess the `(left, right)` endpoints of the image content at this row, bounded by extra black
/// pixels. Operates in one pass and consumes the row. If the entire row consists of black pixels,
/// the left bound will be the largest possible index of the array.
fn row_bounds<P>(mut row: Pixels<P>) -> (u32, u32)
where
    P: Pixel<Subpixel = u8>,
{
    // First row segment: find the leftmost position at which a non-black pixel appears
    let mut left_bound = 0;
    while let Some(true) = row.next().map(|p| p.to_luma()[0] < 16) {
        left_bound += 1;
    }

    // Second row segment: find the rightmost position at which a non-black pixel appears
    let mut right_bound = 0;
    let mut cursor = left_bound;
    while row.len() != 0 {
        // Consume a segment of non-black pixels
        while let Some(false) = row.next().map(|p| p.to_luma()[0] < 16) {
            cursor += 1;
        }

        // The end of that segment is our current guess for the right bound
        right_bound = cursor;

        // Consume the following segment of black pixels
        while let Some(true) = row.next().map(|p| p.to_luma()[0] < 16) {
            cursor += 1
        }
    }

    (left_bound, right_bound)
}

/// Crop out bordering black pixels
fn auto_crop<P>(img: &mut ImageBuffer<P, Vec<u8>>) -> SubImage<&mut ImageBuffer<P, Vec<u8>>>
where
    P: Pixel<Subpixel = u8>,
{
    let img_width = img.width();
    let mut rows = img.rows();

    let mut right_crop = 0;
    let mut top_crop = 0;
    let mut left_crop = img_width;
    let mut bot_crop = 0;

    // First col segment: find the topmost position at which a non-black row appears
    while let Some(true) = rows.next().map(|r| row_bounds(r).0 == img_width) {
        top_crop += 1;
    }

    // Second col segment: find the botmost position at which a non-black row appears
    let mut cursor = top_crop;
    while rows.len() != 0 {
        // Consume a segment of non-black rows
        while let Some(row) = rows.next() {
            let (row_left_crop, row_right_crop) = row_bounds(row);
            if row_right_crop > right_crop {
                right_crop = row_right_crop;
            }

            if row_left_crop < left_crop {
                left_crop = row_left_crop;
            }

            cursor += 1;

            if row_left_crop == img_width {
                break;
            }
        }

        // The end of that segment is our current guess for the bot bound
        bot_crop = cursor;

        // Consume the following segment of black rows
        while let Some(true) = rows.next().map(|r| row_bounds(r).0 == img_width) {
            cursor += 1;
        }
    }

    imageops::crop(
        img,
        left_crop,
        top_crop,
        right_crop - left_crop,
        bot_crop - top_crop,
    )
}

fn auto_brighten<P>(img: &mut ImageBuffer<P, Vec<u8>>, luma: u8)
where
    P: Pixel<Subpixel = u8>,
{
    let factor = (u8::MAX - luma) as f32 / u8::MAX as f32;

    let scale = |channel: &mut u8| {
        *channel = u8::MAX - ((u8::MAX - *channel) as f32 * factor) as u8;
    };

    img.pixels_mut()
        .map(|p| p.channels_mut())
        .flatten()
        .for_each(scale);
}

fn most_recent_file(dir: &Path) -> Result<PathBuf> {
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    struct FileEntryHelper {
        created: SystemTime,
        path: PathBuf,
        is_file: bool,
    }

    impl FileEntryHelper {
        // Unwrap all of the inner `Results` in a `DirEntry` and shove the wanted properties into a
        // new `FileEntryHelper`.
        fn from_dir_entry(dir_entry: DirEntry) -> Result<Self> {
            let path = dir_entry.path();

            let metadata = dir_entry.metadata().with_context(|| format!("couldn't get metadata of file {path:?}"))?;

            let created = metadata.created().with_context(|| format!("couldn't get creation date of file {path:?}"))?;

            Ok(Self { created, path, is_file: metadata.is_file() })
        }
    }

    let entries = fs::read_dir(dir).context("failed to read directory")?;

    let mut files = Vec::new();
    for entry in entries {
        match entry.context("couldn't read file in given directory").and_then(FileEntryHelper::from_dir_entry) {
            Ok(entry) => {
                if entry.is_file && entry.path.file_stem().map_or(true, |s| !s.to_string_lossy().contains("vlc-print-out")) {
                    files.push(entry);
                }
            }
            Err(e) => {
                let mut chain = e.chain();
                eprintln!("warning: {}\n", chain.next().unwrap());
                for err in chain {
                    eprintln!("caused by: {err}\n");
                }
            }
        }
    }

    let latest_file = files.into_iter().max_by_key(|f| f.created);

    Ok(latest_file.context("no valid files in directory")?.path)
}

fn go() -> Result<()> {
    let args: Args = argh::from_env();

    let mut pb = ProgressBar::new(6);
    pb.format("[=> ]");
    pb.show_percent = false;
    pb.show_speed = false;
    pb.show_time_left = false;
    pb.message("finding image ");
    pb.tick();

    let orig_path = most_recent_file(&args.snapshot_dir).context("failed to get most recent file in given directory")?;

    pb.message("opening image ");
    pb.inc();

    let mut img = ImageReader::open(&orig_path)
        .with_context(|| format!("failed to read {:?}", orig_path))?
        .decode()
        .with_context(|| format!("failed to decode {:?}", orig_path))?
        .into_rgb8();

    pb.message("cropping image ");
    pb.inc();

    let mut cropped_img = auto_crop(&mut img).to_image();

    pb.message("brightening image ");
    pb.inc();

    if args.luma != 0 {
        auto_brighten(&mut cropped_img, args.luma);
    }

    pb.message("writing image ");
    pb.inc();

    let orig_name = orig_path
        .file_stem()
        .unwrap_or_default()
        .to_str()
        .context("invalid UTF-8 in file name")?
        .to_owned();

    let orig_extension = orig_path
        .extension()
        .unwrap_or_default()
        .to_str()
        .context("invalid UTF-8 in file name")?;

    let new_path = orig_path
        .with_file_name(orig_name + "-vlc-print-out")
        .with_extension(orig_extension);

    cropped_img
        .save(&new_path)
        .with_context(|| format!("failed to save cropped image to {:?}", new_path))?;

    pb.message("printing image ");
    pb.inc();

    if cfg!(target_os = "windows") {
        Command::new("mspaint").arg("/p").arg(new_path).output().context("couldn't print through mspaint")?;
    } else {
        bail!("it looks like you aren't running this on Windows");
    }

    pb.inc();

    Ok(())
}

fn main() {
    match go() {
        Ok(_) => (),
        Err(e) => {
            let mut chain = e.chain();
            println!("\n\nerror: {}\n", chain.next().unwrap());
            if chain.len() != 0 {
                println!("caused by:");
                for cause in chain {
                    println!("\t{cause}");
                }
                println!("");
            }

            println!("press the 'x' button in the upper right corner of this window to close it");
            std::thread::park();
        }
    }
}
