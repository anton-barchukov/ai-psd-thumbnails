//! Shared helper for draining an IStream into a Vec<u8>.
//!
//! Both ThumbProvider and PreviewProvider need identical stream-draining
//! logic; this module keeps it in one place (DRY).

use windows::Win32::Foundation::E_FAIL;
use windows::Win32::System::Com::{IStream, STATFLAG_NONAME, STATSTG};

/// Maximum stream size accepted to guard against hostile / oversized files (2 GiB).
const MAX_STREAM_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// Read-chunk size when draining an IStream. 1 MiB keeps the number of COM
/// round-trips low for multi-hundred-MB PSB files.
const READ_CHUNK: u32 = 1024 * 1024;

/// Drain an `IStream` into a `Vec<u8>`, up to `MAX_STREAM_BYTES`.
///
/// Loops until `ISequentialStream::Read` reports S_FALSE or returns 0 bytes.
/// Returns `E_FAIL` if the stream is too large or a read error occurs.
pub(crate) fn read_stream_all(stream: &IStream) -> windows::core::Result<Vec<u8>> {
    // Reserve the exact stream size up front when the stream reports it, so a
    // 250 MB file doesn't grow through doubling to a 512 MB allocation. A bogus
    // or oversized hint is ignored; the cap below still applies while reading.
    let mut stat = STATSTG::default();
    let size_hint = unsafe { stream.Stat(&mut stat, STATFLAG_NONAME) }
        .ok()
        .map(|_| stat.cbSize)
        .filter(|&n| n > 0 && n <= MAX_STREAM_BYTES as u64)
        .unwrap_or(0) as usize;

    let mut data: Vec<u8> = Vec::with_capacity(size_hint);
    let mut chunk = vec![0u8; READ_CHUNK as usize];

    loop {
        let mut cb_read: u32 = 0;
        let hr = unsafe {
            stream.Read(
                chunk.as_mut_ptr() as *mut std::ffi::c_void,
                READ_CHUNK,
                Some(&mut cb_read),
            )
        };

        if cb_read > 0 {
            // Clamp cb_read to the actual chunk buffer size: a buggy or hostile
            // IStream implementation may report more bytes read than the buffer
            // can hold, which would cause a slice panic.
            let cb = (cb_read as usize).min(READ_CHUNK as usize);
            let total = data.len() + cb;
            if total > MAX_STREAM_BYTES {
                return Err(windows::core::Error::from_hresult(E_FAIL));
            }
            data.extend_from_slice(&chunk[..cb]);
        }

        // S_OK (0) means more data may follow; S_FALSE (1) means end-of-stream.
        if hr.is_err() {
            return Err(windows::core::Error::from_hresult(hr));
        }
        if hr.0 != 0 || cb_read == 0 {
            break;
        }
    }

    Ok(data)
}
