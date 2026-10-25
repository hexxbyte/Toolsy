//! Recognises downloaded files by their contents and unpacks archives.
//! Supported: zip, 7z, tar and single-file gz/bz2/xz/zst streams (and tarballs of those).

use std::{
	fs::{self, File},
	io::{self, Read},
	path::{Path, PathBuf},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Compression {
	None,
	Gz,
	Bz2,
	Xz,
	Zst,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
	Exe,
	Msi,
	Zip,
	SevenZ,
	/// A tar archive, possibly compressed.
	Tar(Compression),
	/// A single compressed file (not a tarball).
	Single(Compression),
	Unknown,
}

impl Kind {
	pub const fn is_archive(self) -> bool {
		matches!(self, Self::Zip | Self::SevenZ | Self::Tar(_) | Self::Single(_))
	}
}

const HEAD: usize = 512;

fn is_tar(head: &[u8]) -> bool {
	head.get(257..262) == Some(b"ustar")
}

/// Classifies a file from its first bytes. Compressed streams are reported as `Single`;
/// `sniff` looks inside them to tell tarballs apart.
fn classify(head: &[u8]) -> Kind {
	if head.starts_with(b"MZ") {
		Kind::Exe
	} else if head.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
		Kind::Msi
	} else if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
		Kind::Zip
	} else if head.starts_with(&[b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C]) {
		Kind::SevenZ
	} else if head.starts_with(&[0x1F, 0x8B]) {
		Kind::Single(Compression::Gz)
	} else if head.starts_with(b"BZh") {
		Kind::Single(Compression::Bz2)
	} else if head.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
		Kind::Single(Compression::Xz)
	} else if head.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
		Kind::Single(Compression::Zst)
	} else if is_tar(head) {
		Kind::Tar(Compression::None)
	} else {
		Kind::Unknown
	}
}

fn decoder(c: Compression, file: File) -> io::Result<Box<dyn Read>> {
	Ok(match c {
		Compression::None => Box::new(file),
		Compression::Gz => Box::new(flate2::read::GzDecoder::new(file)),
		Compression::Bz2 => Box::new(bzip2::read::BzDecoder::new(file)),
		Compression::Xz => Box::new(xz2::read::XzDecoder::new(file)),
		Compression::Zst => Box::new(zstd::stream::read::Decoder::new(file)?),
	})
}

fn read_head(mut r: impl Read) -> io::Result<Vec<u8>> {
	let mut head = Vec::with_capacity(HEAD);
	r.by_ref().take(HEAD as u64).read_to_end(&mut head)?;
	Ok(head)
}

/// Works out what a downloaded file is from its contents.
pub fn sniff(path: &Path) -> io::Result<Kind> {
	let kind = classify(&read_head(File::open(path)?)?);
	let Kind::Single(c) = kind else { return Ok(kind) };
	// Look inside the compressed stream; a damaged one is treated as a plain stream.
	let inner = read_head(decoder(c, File::open(path)?)?).unwrap_or_default();
	Ok(if is_tar(&inner) { Kind::Tar(c) } else { kind })
}

/// Joins an archive entry name onto `dest`, refusing anything that could escape it.
fn safe_join(dest: &Path, entry: &str) -> Option<PathBuf> {
	let mut out = dest.to_path_buf();
	for part in entry.split(['/', '\\']) {
		match part {
			"" | "." => {}
			".." => return None,
			p if p.contains(':') => return None,
			p => out.push(p),
		}
	}
	Some(out)
}

/// Replaces characters Windows does not allow in a file or folder name.
pub fn sanitize(name: &str) -> String {
	let cleaned: String =
		name.chars().map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }).collect();
	let cleaned = cleaned.trim().trim_matches('.').trim();
	if cleaned.is_empty() { "package".to_string() } else { cleaned.to_string() }
}

fn write_entry(target: &Path, reader: &mut dyn Read) -> io::Result<()> {
	if let Some(parent) = target.parent() {
		fs::create_dir_all(parent)?;
	}
	io::copy(reader, &mut File::create(target)?)?;
	Ok(())
}

fn unsafe_entry(name: &str) -> String {
	format!("Refusing to extract \"{name}\": it would write outside the destination folder")
}

/// Extracts `path` (of the given archive `kind`) into `dest`, overwriting existing files.
/// `single_name` names the output of a bare compressed file. Returns the number of files written.
pub fn extract(path: &Path, kind: Kind, dest: &Path, single_name: &str) -> Result<usize, String> {
	fs::create_dir_all(dest).map_err(|e| format!("Could not create {}: {e}", dest.display()))?;
	let result = match kind {
		Kind::Zip => extract_zip(path, dest),
		Kind::SevenZ => extract_7z(path, dest),
		Kind::Tar(c) => extract_tar(path, c, dest),
		Kind::Single(c) => extract_single(path, c, dest, single_name),
		Kind::Exe | Kind::Msi | Kind::Unknown => return Err("This file is not an archive".into()),
	};
	result.map_err(|e| e.to_string())
}

#[derive(Debug)]
struct Error(String);

impl std::fmt::Display for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(&self.0)
	}
}

impl<E: std::error::Error> From<E> for Error {
	fn from(e: E) -> Self {
		Self(e.to_string())
	}
}

fn extract_zip(path: &Path, dest: &Path) -> Result<usize, Error> {
	let mut archive = zip::ZipArchive::new(File::open(path)?)?;
	let mut count = 0;
	for i in 0..archive.len() {
		let mut entry = archive.by_index(i)?;
		let name = entry.name()?.into_owned();
		let target = safe_join(dest, &name).ok_or_else(|| Error(unsafe_entry(&name)))?;
		if entry.is_dir() || name.ends_with('/') {
			fs::create_dir_all(&target)?;
		} else {
			write_entry(&target, &mut entry)?;
			count += 1;
		}
	}
	Ok(count)
}

fn extract_7z(path: &Path, dest: &Path) -> Result<usize, Error> {
	let mut reader = sevenz_rust2::ArchiveReader::new(File::open(path)?, sevenz_rust2::Password::empty())?;
	let mut count = 0;
	reader.for_each_entries(|entry, data| {
		let target = safe_join(dest, entry.name())
			.ok_or_else(|| sevenz_rust2::Error::Other(unsafe_entry(entry.name()).into()))?;
		if entry.is_directory() {
			fs::create_dir_all(&target)?;
		} else {
			write_entry(&target, data)?;
			count += 1;
		}
		Ok(true)
	})?;
	Ok(count)
}

fn extract_tar(path: &Path, c: Compression, dest: &Path) -> Result<usize, Error> {
	let mut archive = tar::Archive::new(decoder(c, File::open(path)?)?);
	let mut count = 0;
	for entry in archive.entries()? {
		let mut entry = entry?;
		let name = entry.path()?.to_string_lossy().into_owned();
		let target = safe_join(dest, &name).ok_or_else(|| Error(unsafe_entry(&name)))?;
		let kind = entry.header().entry_type();
		if kind.is_dir() {
			fs::create_dir_all(&target)?;
		} else if kind.is_file() {
			write_entry(&target, &mut entry)?;
			count += 1;
		}
		// Links, devices and the like are skipped: they can point outside the destination.
	}
	Ok(count)
}

fn extract_single(path: &Path, c: Compression, dest: &Path, name: &str) -> Result<usize, Error> {
	let mut reader = decoder(c, File::open(path)?)?;
	write_entry(&dest.join(sanitize(name)), &mut reader)?;
	Ok(1)
}
