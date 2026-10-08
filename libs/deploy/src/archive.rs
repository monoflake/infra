//! What an image archive is, read before it is loaded: the ids Docker gives the image it holds, so
//! a node can tell an image it already runs without loading it again. See
//! spec/architecture/host.md.

use serde::Deserialize;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// The most the two small files read from an archive may weigh; anything bigger is not what a
/// build writes.
const LARGEST: u64 = 1024 * 1024;

/// The ids Docker may give the image in the archive at `path`: the digest of its manifest, which
/// Docker's containerd store takes as the image's id, and the digest of its config, which the
/// older store does. Each is `sha256:<hex>`; none for an archive holding neither file. The tar is
/// walked by its headers, its layers skipped rather than read.
pub fn identities(path: &Path) -> std::io::Result<Vec<String>> {
	let mut file = std::fs::File::open(path)?;
	let (mut index, mut manifest) = (None, None);
	let mut header = [0u8; 512];
	loop {
		if file.read_exact(&mut header).is_err() || header.iter().all(|byte| *byte == 0) {
			break;
		}
		let name = entry_name(&header);
		let size = octal(&header[124..136]);
		let padded = size.div_ceil(512) * 512;
		match name.trim_start_matches("./") {
			"index.json" if size <= LARGEST => index = Some(read(&mut file, size, padded)?),
			"manifest.json" if size <= LARGEST => manifest = Some(read(&mut file, size, padded)?),
			_ => {
				let skip = i64::try_from(padded).unwrap_or(i64::MAX);
				file.seek(SeekFrom::Current(skip))?;
			}
		}
	}
	Ok(ids(index.as_deref(), manifest.as_deref()))
}

/// The two ids from the archive's `index.json` and `manifest.json`, when each says one.
fn ids(index: Option<&[u8]>, manifest: Option<&[u8]>) -> Vec<String> {
	#[derive(Deserialize)]
	struct Described {
		digest: String,
	}
	#[derive(Deserialize)]
	struct Index {
		manifests: Vec<Described>,
	}
	#[derive(Deserialize)]
	struct Saved {
		#[serde(rename = "Config")]
		config: String,
	}
	let mut found = Vec::new();
	let index = index.and_then(|bytes| serde_json::from_slice::<Index>(bytes).ok());
	if let Some(first) = index.and_then(|index| index.manifests.into_iter().next()) {
		found.push(first.digest);
	}
	let saved = manifest.and_then(|bytes| serde_json::from_slice::<Vec<Saved>>(bytes).ok());
	if let Some(config) = saved.and_then(|saved| saved.into_iter().next()) {
		// `blobs/sha256/<hex>` in an OCI layout, `<hex>.json` in the older one.
		let file = config.config.rsplit('/').next().unwrap_or_default();
		let hex = file.trim_end_matches(".json");
		if hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
			found.push(format!("sha256:{hex}"));
		}
	}
	found.retain(|id| id.starts_with("sha256:"));
	found
}

fn read(file: &mut std::fs::File, size: u64, padded: u64) -> std::io::Result<Vec<u8>> {
	let mut bytes = vec![0; usize::try_from(size).unwrap_or_default()];
	file.read_exact(&mut bytes)?;
	file.seek(SeekFrom::Current(i64::try_from(padded - size).unwrap_or_default()))?;
	Ok(bytes)
}

/// An entry's name: ustar's prefix and name joined, each ending at its first NUL.
fn entry_name(header: &[u8; 512]) -> String {
	let field = |bytes: &[u8]| {
		let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
		String::from_utf8_lossy(&bytes[..end]).into_owned()
	};
	let (name, prefix) = (field(&header[..100]), field(&header[345..500]));
	if prefix.is_empty() { name } else { format!("{prefix}/{name}") }
}

/// A tar number: octal digits, padded with spaces or NULs.
fn octal(bytes: &[u8]) -> u64 {
	let digits = bytes.iter().filter(|byte| (b'0'..=b'7').contains(byte));
	digits.fold(0, |value, digit| value * 8 + u64::from(digit - b'0'))
}

#[cfg(test)]
mod tests {
	use super::identities;
	use std::io::Write;

	/// A tar of `entries`, as `docker buildx --output type=docker` lays one out.
	fn tar(entries: &[(&str, &[u8])]) -> tempfile::NamedTempFile {
		let mut file = tempfile::NamedTempFile::new().unwrap();
		for (name, bytes) in entries {
			let mut header = [0u8; 512];
			header[..name.len()].copy_from_slice(name.as_bytes());
			header[100..108].copy_from_slice(b"0000644\0");
			let size = format!("{:011o}\0", bytes.len());
			header[124..136].copy_from_slice(size.as_bytes());
			header[156] = b'0';
			header[257..263].copy_from_slice(b"ustar\0");
			// The checksum is computed with its own field as spaces.
			header[148..156].copy_from_slice(b"        ");
			let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
			header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
			file.write_all(&header).unwrap();
			file.write_all(bytes).unwrap();
			file.write_all(&vec![0; bytes.len().div_ceil(512) * 512 - bytes.len()]).unwrap();
		}
		file.write_all(&[0; 1024]).unwrap();
		file
	}

	const MANIFEST: &str = "c9b671a440af91246fae9ee7cdafa8a368d807554aba21eb7d0bc1014dd99e95";
	const CONFIG: &str = "569f0bf836b49c3112b7efa7d503d17133f8edfdd8ab039a1ffa627143002ece";

	#[test]
	fn an_oci_archive_names_its_manifest_and_its_config_past_its_layers() {
		let index = format!(r#"{{"schemaVersion":2,"manifests":[{{"digest":"sha256:{MANIFEST}"}}]}}"#);
		let saved = format!(r#"[{{"Config":"blobs/sha256/{CONFIG}","Layers":[]}}]"#);
		let layer = vec![7u8; 5000];
		let archive = tar(&[
			("blobs/sha256/aaaa", &layer),
			("index.json", index.as_bytes()),
			("manifest.json", saved.as_bytes()),
			("oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#),
		]);
		let ids = identities(archive.path()).unwrap();
		assert_eq!(ids, [format!("sha256:{MANIFEST}"), format!("sha256:{CONFIG}")]);
	}

	#[test]
	fn an_older_archive_names_its_config_alone_and_anything_else_nothing() {
		let saved = format!(r#"[{{"Config":"{CONFIG}.json","RepoTags":["geo:local"]}}]"#);
		let archive = tar(&[("manifest.json", saved.as_bytes())]);
		assert_eq!(identities(archive.path()).unwrap(), [format!("sha256:{CONFIG}")]);
		let nothing = tar(&[("image.tar", b"not an image")]);
		assert!(identities(nothing.path()).unwrap().is_empty());
		let unreadable = tar(&[("index.json", b"{"), ("manifest.json", b"[{\"Config\":\"short\"}]")]);
		assert!(identities(unreadable.path()).unwrap().is_empty());
	}
}
