//! Large files written and read front to back without staying in the page cache, which counts
//! against the container's own memory ceiling. See spec/architecture/host.md, "A large file is
//! written in chunks that leave the page cache as they land".

use bytes::Bytes;
use futures_util::Stream;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

/// How much is written or read before that range is flushed and dropped from the cache.
pub const CHUNK: u64 = 8 << 20;

/// How much an [`AsyncWriter`] gathers before a blocking thread writes it, and how much one read
/// of [`read`] takes.
const BUFFER: usize = 1 << 20;

/// A file written front to back, each [`CHUNK`] flushed and dropped once it lands. Only
/// [`Writer::finish`] does so for the last, partial one.
pub struct Writer {
	file: File,
	mark: Mark,
}

impl Writer {
	pub fn create(path: &Path) -> io::Result<Self> {
		Ok(Self::with(File::create(path)?, CHUNK))
	}

	fn with(file: File, chunk: u64) -> Self {
		Self { file, mark: Mark::new(chunk) }
	}

	pub fn finish(mut self) -> io::Result<()> {
		self.close()
	}

	fn close(&mut self) -> io::Result<()> {
		self.file.flush()?;
		self.mark.settle(&self.file, true)
	}
}

impl Write for Writer {
	fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
		let written = self.file.write(bytes)?;
		self.mark.advance(&self.file, written, true)?;
		Ok(written)
	}

	fn flush(&mut self) -> io::Result<()> {
		self.file.flush()
	}
}

/// A file read front to back, each [`CHUNK`] dropped from the cache once read, and the rest at
/// the end of the file.
pub struct Reader {
	file: File,
	mark: Mark,
}

impl Reader {
	pub fn open(path: &Path) -> io::Result<Self> {
		Ok(Self::with(File::open(path)?, CHUNK))
	}

	fn with(file: File, chunk: u64) -> Self {
		Self { file, mark: Mark::new(chunk) }
	}
}

impl Read for Reader {
	fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
		let read = self.file.read(buffer)?;
		match read {
			0 if !buffer.is_empty() => self.mark.settle(&self.file, false)?,
			_ => self.mark.advance(&self.file, read, false)?,
		}
		Ok(read)
	}
}

/// A [`Writer`] fed from async code: what it is given is gathered and written on a blocking
/// thread, as `tokio::fs::File` does.
pub struct AsyncWriter {
	writer: Option<Writer>,
	pending: Vec<u8>,
}

impl AsyncWriter {
	pub async fn create(path: &Path) -> io::Result<Self> {
		let path = path.to_owned();
		let writer = blocking(move || Writer::create(&path)).await?;
		Ok(Self { writer: Some(writer), pending: Vec::with_capacity(BUFFER) })
	}

	pub async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
		self.pending.extend_from_slice(bytes);
		if self.pending.len() >= BUFFER {
			self.drain().await?;
		}
		Ok(())
	}

	pub async fn finish(mut self) -> io::Result<()> {
		self.drain().await?;
		let writer = self.take()?;
		blocking(move || writer.finish()).await
	}

	async fn drain(&mut self) -> io::Result<()> {
		let bytes = std::mem::replace(&mut self.pending, Vec::with_capacity(BUFFER));
		let mut writer = self.take()?;
		self.writer = Some(blocking(move || writer.write_all(&bytes).map(|()| writer)).await?);
		Ok(())
	}

	fn take(&mut self) -> io::Result<Writer> {
		self.writer.take().ok_or_else(|| io::Error::other("an earlier write to this file failed"))
	}
}

/// `path` as a stream for Docker to load, read through a [`Reader`] on a blocking thread.
pub async fn read(
	path: &Path,
) -> io::Result<impl Stream<Item = io::Result<Bytes>> + Send + 'static> {
	let path = path.to_owned();
	let reader = blocking(move || Reader::open(&path)).await?;
	Ok(futures_util::stream::try_unfold(reader, |mut reader| async move {
		let (reader, bytes) = blocking(move || {
			let mut buffer = vec![0; BUFFER];
			let read = reader.read(&mut buffer)?;
			buffer.truncate(read);
			Ok((reader, buffer))
		})
		.await?;
		Ok((!bytes.is_empty()).then(|| (Bytes::from(bytes), reader)))
	}))
}

async fn blocking<T, F>(work: F) -> io::Result<T>
where
	T: Send + 'static,
	F: FnOnce() -> io::Result<T> + Send + 'static,
{
	tokio::task::spawn_blocking(work).await.map_err(io::Error::other)?
}

/// How far a file has been handled, and how much of that is out of the cache.
struct Mark {
	chunk: u64,
	done: u64,
	dropped: u64,
	#[cfg(test)]
	calls: Vec<Call>,
}

#[cfg(test)]
#[derive(Debug, PartialEq)]
enum Call {
	Flush(u64, u64),
	Drop(u64, u64),
}

impl Mark {
	fn new(chunk: u64) -> Self {
		Self {
			chunk,
			done: 0,
			dropped: 0,
			#[cfg(test)]
			calls: Vec::new(),
		}
	}

	fn advance(&mut self, file: &File, bytes: usize, flush: bool) -> io::Result<()> {
		self.done += bytes as u64;
		match self.done - self.dropped >= self.chunk {
			true => self.settle(file, flush),
			false => Ok(()),
		}
	}

	/// Everything handled since the last drop: written back first when it was written, since a
	/// dirty page is not dropped, then dropped.
	fn settle(&mut self, file: &File, flush: bool) -> io::Result<()> {
		let (offset, length) = (self.dropped, self.done - self.dropped);
		if length == 0 {
			return Ok(());
		}
		if flush {
			write_back(file, offset, length)?;
			#[cfg(test)]
			self.calls.push(Call::Flush(offset, length));
		}
		drop_cached(file, offset, length)?;
		#[cfg(test)]
		self.calls.push(Call::Drop(offset, length));
		self.dropped = self.done;
		Ok(())
	}
}

/// Write the range out and wait for it, without the metadata an `fdatasync` would add.
#[cfg(target_os = "linux")]
fn write_back(file: &File, offset: u64, length: u64) -> io::Result<()> {
	use std::os::fd::AsRawFd;
	let flags = libc::SYNC_FILE_RANGE_WAIT_BEFORE
		| libc::SYNC_FILE_RANGE_WRITE
		| libc::SYNC_FILE_RANGE_WAIT_AFTER;
	// SAFETY: a plain call on a descriptor `file` keeps open; the kernel touches no memory of ours.
	match unsafe { libc::sync_file_range(file.as_raw_fd(), offset as _, length as _, flags) } {
		0 => Ok(()),
		_ => Err(io::Error::last_os_error()),
	}
}

#[cfg(target_os = "linux")]
fn drop_cached(file: &File, offset: u64, length: u64) -> io::Result<()> {
	use std::os::fd::AsRawFd;
	let advice = libc::POSIX_FADV_DONTNEED;
	// SAFETY: as above. The error comes back as the result rather than through errno.
	match unsafe { libc::posix_fadvise(file.as_raw_fd(), offset as _, length as _, advice) } {
		0 => Ok(()),
		errno => Err(io::Error::from_raw_os_error(errno)),
	}
}

// A Mac in development has no ceiling to stay under, so there is nothing to drop there.
#[cfg(not(target_os = "linux"))]
fn write_back(_: &File, _: u64, _: u64) -> io::Result<()> {
	Ok(())
}

#[cfg(not(target_os = "linux"))]
fn drop_cached(_: &File, _: u64, _: u64) -> io::Result<()> {
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use futures_util::TryStreamExt;

	fn bytes(length: usize) -> Vec<u8> {
		(0..length).map(|at| (at % 251) as u8).collect()
	}

	fn written(chunk: u64, content: &[u8], piece: usize) -> (Vec<Call>, Vec<u8>) {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("file");
		let mut writer = Writer::with(File::create(&path).unwrap(), chunk);
		for piece in content.chunks(piece) {
			writer.write_all(piece).unwrap();
		}
		writer.close().unwrap();
		(std::mem::take(&mut writer.mark.calls), std::fs::read(&path).unwrap())
	}

	#[test]
	fn a_writer_settles_each_chunk_and_the_partial_one_on_close() {
		let content = bytes(25);
		let (calls, written) = written(10, &content, 4);
		let expected = vec![
			Call::Flush(0, 12),
			Call::Drop(0, 12),
			Call::Flush(12, 12),
			Call::Drop(12, 12),
			Call::Flush(24, 1),
			Call::Drop(24, 1),
		];
		assert_eq!(calls, expected);
		assert_eq!(written, content);
	}

	#[test]
	fn a_writer_ending_on_a_boundary_settles_nothing_more() {
		let content = bytes(20);
		let (calls, written) = written(10, &content, 5);
		let expected =
			vec![Call::Flush(0, 10), Call::Drop(0, 10), Call::Flush(10, 10), Call::Drop(10, 10)];
		assert_eq!(calls, expected);
		assert_eq!(written, content);
	}

	#[test]
	fn a_reader_drops_each_chunk_and_the_rest_at_the_end() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("file");
		let content = bytes(25);
		std::fs::write(&path, &content).unwrap();
		let mut reader = Reader::with(File::open(&path).unwrap(), 10);
		let mut read = Vec::new();
		let mut buffer = [0; 6];
		loop {
			match reader.read(&mut buffer).unwrap() {
				0 => break,
				count => read.extend_from_slice(&buffer[..count]),
			}
		}
		assert_eq!(read, content);
		let expected = vec![Call::Drop(0, 12), Call::Drop(12, 12), Call::Drop(24, 1)];
		assert_eq!(reader.mark.calls, expected);
	}

	#[tokio::test]
	async fn the_async_writer_and_the_stream_carry_the_bytes_unchanged() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("file");
		let content = bytes(3 * BUFFER + 12_345);
		let mut writer = AsyncWriter::create(&path).await.unwrap();
		for piece in content.chunks(16_384) {
			writer.write(piece).await.unwrap();
		}
		writer.finish().await.unwrap();
		assert_eq!(std::fs::read(&path).unwrap(), content);
		let streamed: Vec<Bytes> = read(&path).await.unwrap().try_collect().await.unwrap();
		assert_eq!(streamed.concat(), content);
	}
}
