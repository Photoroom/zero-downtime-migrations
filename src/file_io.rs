use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::Path;

#[derive(Debug)]
pub(crate) enum ReadFileError {
    Io(io::Error),
    TooLarge { size: u64, max: u64 },
}

impl From<io::Error> for ReadFileError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Read a bounded UTF-8 regular file without following symlinks on Unix.
/// The handle is opened once, then inspected and read, avoiding check/open
/// races and preventing FIFOs, devices, and oversized inputs from blocking or
/// exhausting memory.
pub(crate) fn read_bounded_regular_file(
    path: &Path,
    max_size: u64,
) -> std::result::Result<String, ReadFileError> {
    let link_metadata = std::fs::symlink_metadata(path)?;
    if !link_metadata.file_type().is_file() {
        return Err(invalid_file_type().into());
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    read_bounded_handle(file, max_size)
}

fn read_bounded_handle(file: File, max_size: u64) -> std::result::Result<String, ReadFileError> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid_file_type().into());
    }
    if metadata.len() > max_size {
        return Err(ReadFileError::TooLarge {
            size: metadata.len(),
            max: max_size,
        });
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_size + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_size {
        return Err(ReadFileError::TooLarge {
            size: bytes.len() as u64,
            max: max_size,
        });
    }
    String::from_utf8(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error).into())
}

fn invalid_file_type() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "refusing to read non-regular file (symlink, fifo, device, etc.)",
    )
}
