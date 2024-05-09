use anyhow::{ensure, Context, Result};
use futures::stream::StreamExt;
use memmap2::{Mmap, MmapOptions};
use nix::fcntl::posix_fadvise;
use nix::fcntl::PosixFadviseAdvice;
use nix::sys::mman::{madvise, MmapAdvise};
use nix::unistd::{sysconf, SysconfVar};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;
use tokio::fs;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReadDirStream;

// Reverse engineered from looking at a bunch of ARW files. Obviously not stable, tested on Sony a1
// with 1.31 firmware. Can be extracted by iterating through EXIF, but that's much slower, and
// these are static.
const OFFSET_POSITION: usize = 0x21c18;
const LENGTH_POSITION: usize = 0x21c24;

const fn is_jpeg_soi(buf: &[u8]) -> bool {
    buf[0] == 0xff && buf[1] == 0xd8
}

unsafe fn madvise_aligned(addr: *mut u8, length: usize, advice: MmapAdvise) -> Result<()> {
    let page_size: usize = sysconf(SysconfVar::PAGE_SIZE).unwrap().unwrap() as usize;

    let page_aligned_start = (addr as usize) & !(page_size - 1);

    let original_end = addr as usize + length;
    let page_aligned_end = (original_end + page_size - 1) & !(page_size - 1);

    let aligned_length = page_aligned_end - page_aligned_start;
    let aligned_addr = page_aligned_start as *mut _;
    let aligned_nonnull = NonNull::new(aligned_addr)
        .ok_or_else(|| anyhow::anyhow!("Failed to convert aligned address to NonNull"))?;

    madvise(aligned_nonnull, aligned_length, advice).context("Failed to madvise()")
}

async fn mmap_arw(arw_fd: i32) -> Result<Mmap> {
    // We only access a small part of the file, don't read in more than necessary.
    posix_fadvise(arw_fd, 0, 0, PosixFadviseAdvice::POSIX_FADV_RANDOM).unwrap();
    posix_fadvise(
        arw_fd,
        OFFSET_POSITION as i64,
        (LENGTH_POSITION - OFFSET_POSITION + 4) as i64,
        PosixFadviseAdvice::POSIX_FADV_WILLNEED,
    )
    .unwrap();

    let arw_buf = unsafe { MmapOptions::new().map(arw_fd).unwrap() };

    let base_length = arw_buf.len();
    ensure!(base_length > LENGTH_POSITION + 4);
    unsafe {
        madvise_aligned(
            arw_buf.as_ptr() as *mut _,
            base_length,
            MmapAdvise::MADV_RANDOM,
        )
        .unwrap();
        let advised_ptr = arw_buf.as_ptr().add(OFFSET_POSITION);
        madvise_aligned(
            advised_ptr as *mut _,
            LENGTH_POSITION - OFFSET_POSITION + 4,
            MmapAdvise::MADV_WILLNEED,
        )
        .unwrap();
    }

    Ok(arw_buf)
}

fn extract_jpeg(arw_fd: i32, arw_buf: &[u8]) -> Result<&[u8]> {
    let jpeg_offset: usize =
        u32::from_le_bytes(arw_buf[OFFSET_POSITION..OFFSET_POSITION + 4].try_into()?).try_into()?;
    let jpeg_sz: usize =
        u32::from_le_bytes(arw_buf[LENGTH_POSITION..LENGTH_POSITION + 4].try_into()?).try_into()?;

    posix_fadvise(
        arw_fd,
        jpeg_offset as i64,
        jpeg_sz as i64,
        PosixFadviseAdvice::POSIX_FADV_WILLNEED,
    )
    .unwrap();
    unsafe {
        let advised_ptr = arw_buf.as_ptr().add(jpeg_offset);
        madvise_aligned(advised_ptr as *mut _, jpeg_sz, MmapAdvise::MADV_WILLNEED).unwrap();
    }

    ensure!(
        (jpeg_offset + jpeg_sz) <= arw_buf.len(),
        "JPEG data exceeds file size"
    );
    ensure!(
        is_jpeg_soi(&arw_buf[jpeg_offset..]),
        "Missing JPEG SOI marker"
    );

    Ok(&arw_buf[jpeg_offset..jpeg_offset + jpeg_sz])
}

async fn write_jpeg(out_dir: &Path, filename: &str, jpeg_buf: &[u8]) -> Result<()> {
    let mut output_file = out_dir.join(filename);
    output_file.set_extension("jpg");
    println!("{filename}");

    let mut out_file = File::create(&output_file)
        .await
        .context("Failed to open output file")?;
    out_file
        .write_all(jpeg_buf)
        .await
        .context("Failed to write to output file")?;
    Ok(())
}

const MAX_OPEN_FILES: usize = 256;

async fn process_file(entry_path: PathBuf, out_dir: &Path) -> Result<()> {
    let filename = entry_path.file_name().unwrap().to_string_lossy();
    let in_file = File::open(&entry_path)
        .await
        .context("Failed to open ARW file")?;
    let arw_fd = in_file.as_raw_fd();
    let arw_buf = mmap_arw(arw_fd).await?;
    let jpeg_buf = extract_jpeg(arw_fd, &arw_buf)?;
    write_jpeg(out_dir, &filename, jpeg_buf).await?;
    Ok(())
}

async fn process_directory(in_dir: &Path, out_dir: &'static Path) -> Result<()> {
    let ent = fs::read_dir(in_dir)
        .await
        .context("Failed to open input directory")?;
    let mut ent_stream = ReadDirStream::new(ent);
    let semaphore = Arc::new(Semaphore::new(MAX_OPEN_FILES));

    let mut tasks: Vec<JoinHandle<Result<()>>> = Vec::new();

    while let Some(entry) = ent_stream.next().await {
        match entry {
            Ok(e)
                if e.path().extension().map_or(false, |ext| ext == "ARW")
                    && e.metadata().await.unwrap().is_file() =>
            {
                let permit = semaphore.clone().acquire_owned().await.unwrap();
                let task = tokio::spawn(async move {
                    let result = process_file(e.path(), out_dir).await;
                    drop(permit);
                    result
                });
                tasks.push(task);
            }
            _ => continue,
        }
    }

    for task in tasks {
        task.await??;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} input_dir [output_dir]", args[0]);
        std::process::exit(1);
    }

    let in_dir = PathBuf::from(&args[1]);
    let output_dir = if args.len() > 2 {
        PathBuf::from(&args[2])
    } else {
        PathBuf::from(".")
    };
    let output_dir = Box::leak(Box::new(output_dir)); // It's gonna get used for each ARW file and
                                                      // would need a copy for .filter_map(),
                                                      // better to just make it &'static

    fs::create_dir_all(&output_dir)
        .await
        .context("Failed to create output directory")?;
    process_directory(&in_dir, output_dir).await?;

    Ok(())
}
