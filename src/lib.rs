#![warn(
    clippy::all,
    clippy::pedantic,
    future_incompatible,
    macro_use_extern_crate,
    meta_variable_misuse,
    missing_abi,
    nonstandard_style,
    noop_method_call,
    rust_2018_compatibility,
    rust_2018_idioms,
    rust_2021_compatibility,
    trivial_casts,
    unreachable_pub,
    unsafe_code,
    unsafe_op_in_unsafe_fn,
    unused_import_braces,
    unused_lifetimes,
    unused_qualifications,
    unused
)]
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::items_after_statements,
    clippy::many_single_char_names,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::module_name_repetitions,
    clippy::similar_names,
    clippy::struct_excessive_bools,
    clippy::too_many_lines,
    clippy::unreadable_literal
)]

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::{env, thread};

use colored::Colorize;
use dissimilar::{Chunk, diff};
use eyre::{Result, eyre};
use flate2::Compression;
use flate2::bufread::GzDecoder;
use flate2::write::GzEncoder;
use tempfile::{TempDir, tempdir};

#[must_use]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum CursorOp {
    Equal,
    Delete,
    Insert,
}

#[must_use]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct Cursor<'a> {
    idx: usize,
    line: usize,
    s: &'a str,
    printing: bool,
}

impl<'a> Cursor<'a> {
    fn new(s: &'a str) -> Self {
        Self { s, idx: 0, line: 0, printing: false }
    }

    fn advance(&mut self, l: usize, op: CursorOp, print_equal: bool) {
        if op != CursorOp::Equal {
            // Print from beginning of the current line if we haven't already.
            if !self.printing && print_equal {
                print!("{}", &self.s[self.line..self.idx]);
            }
            self.printing = true;
            // Print diff.
            let s = &self.s[self.idx..self.idx + l];
            if op == CursorOp::Delete {
                print!("{}", s.red());
            } else {
                print!("{}", s.green());
            }
        }
        let mut first_newline = l;
        for i in 0..l {
            if self.s.as_bytes()[self.idx + i] == b'\n' {
                if first_newline == l {
                    first_newline = i;
                }
                self.line = self.idx + i + 1;
            }
        }
        // Print rest of the line if necessary.
        if op == CursorOp::Equal && self.printing && print_equal {
            let en = if first_newline == l {
                self.idx + l
            } else {
                self.printing = false;
                self.idx + first_newline + 1
            };
            print!("{}", &self.s[self.idx..en]);
        }
        self.idx += l;
    }
}

#[must_use]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GoldenMode {
    VerifyOnDrop,
    UpdateOnDrop,
    DoNothing,
}

#[must_use]
#[derive(Debug)]
pub struct Golden {
    dir: PathBuf,
    tmp: TempDir,
    paths: Vec<PathBuf>,
    mode: GoldenMode,
}

const BYTE_LIMIT: u64 = 1024;

impl Golden {
    pub fn new(p: impl AsRef<Path>) -> Result<Self> {
        let mode = match env::var("UPDATE_GOLDEN").as_deref() {
            Ok("1") => GoldenMode::UpdateOnDrop,
            _ => GoldenMode::VerifyOnDrop,
        };
        Self::new_with_mode(p, mode)
    }

    pub fn new_with_mode(p: impl AsRef<Path>, mode: GoldenMode) -> Result<Self> {
        Ok(Self { dir: p.as_ref().to_path_buf(), tmp: tempdir()?, paths: Vec::new(), mode })
    }

    pub fn file(&mut self, p: impl AsRef<Path>) -> Result<Box<dyn Write + '_>> {
        self.write_tmp(p.as_ref())
    }

    fn write_tmp(&mut self, p: &Path) -> Result<Box<dyn Write + '_>> {
        Self::validate_rel_path(p)?;
        let tmp_path = self.tmp.path().join(p);
        if let Some(parent) = tmp_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let f = BufWriter::new(File::create(&tmp_path)?);
        self.paths.push(p.to_owned());
        if p.extension().unwrap_or_default() == "gz" {
            Ok(Box::new(GzEncoder::new(f, Compression::best())))
        } else {
            Ok(Box::new(f))
        }
    }

    fn read(p: &Path) -> Result<Box<dyn Read>> {
        let f = BufReader::new(File::open(p)?);
        if p.extension().unwrap_or_default() == "gz" {
            Ok(Box::new(GzDecoder::new(f)))
        } else {
            Ok(Box::new(f))
        }
    }

    fn process_diffs(old: &str, new: &str) -> usize {
        let chunks = diff(old, new);
        let mut diff_count = 0;
        let mut old = Cursor::new(old);
        let mut new = Cursor::new(new);
        for chunk in &chunks {
            match chunk {
                Chunk::Equal(s) => {
                    old.advance(s.len(), CursorOp::Equal, true);
                    new.advance(s.len(), CursorOp::Equal, false); // Don't double print for equal chunks.
                }
                Chunk::Delete(s) => {
                    diff_count += 1;
                    old.advance(s.len(), CursorOp::Delete, true);
                }
                Chunk::Insert(s) => {
                    diff_count += 1;
                    new.advance(s.len(), CursorOp::Insert, false);
                }
            }
        }
        if diff_count != 0 {
            println!();
        }
        diff_count
    }

    fn verify(&self) -> Result<()> {
        for p in &self.paths {
            let golden_path = self.dir.join(p);
            let mut golden = Self::read(&golden_path)?;
            let mut actual = Self::read(&self.tmp.path().join(p))?;

            // Process in chunks of |BYTE_LIMIT|.
            loop {
                let mut old = String::new();
                let mut new = String::new();
                let mut golden_lim = golden.take(BYTE_LIMIT);
                let mut actual_lim = actual.take(BYTE_LIMIT);
                golden_lim.read_to_string(&mut old)?;
                actual_lim.read_to_string(&mut new)?;
                golden = golden_lim.into_inner();
                actual = actual_lim.into_inner();

                if old.is_empty() && new.is_empty() {
                    break;
                }

                let num = Self::process_diffs(&old, &new);
                if num != 0 {
                    return Err(eyre!(
                        "Found at least {} difference(s) in {}! Set UPDATE_GOLDEN=1 to update golden files.",
                        num,
                        p.display()
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_rel_path(p: &Path) -> Result<()> {
        if p.as_os_str().is_empty() {
            return Err(eyre!("invalid golden path (must be non-empty)"));
        }

        let mut last_component = None;
        for c in p.components() {
            last_component = Some(c);
            match c {
                Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                    return Err(eyre!(
                        "invalid golden path (must be relative without '..'): {}",
                        p.display()
                    ));
                }
                Component::CurDir | Component::Normal(_) => {}
            }
        }

        if matches!(last_component, Some(Component::CurDir)) {
            return Err(eyre!("invalid golden path (must not end with '.'): {}", p.display()));
        }
        Ok(())
    }

    fn update(&self) -> Result<()> {
        for p in &self.paths {
            Self::validate_rel_path(p)?;
            let src = self.tmp.path().join(p);
            let dst = self.dir.join(p);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(src, dst)?;
        }
        Ok(())
    }
}

impl Drop for Golden {
    fn drop(&mut self) {
        if thread::panicking() {
            return;
        }
        match self.mode {
            GoldenMode::UpdateOnDrop => {
                self.update().expect("could not update golden files");
            }
            GoldenMode::VerifyOnDrop => {
                self.verify().expect("could not verify golden files");
            }
            GoldenMode::DoNothing => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn new_golden_dir() -> Result<(TempDir, PathBuf)> {
        let tmp = tempdir()?;
        let golden_path = tmp.path().join("golden");
        std::fs::create_dir(&golden_path)?;
        Ok((tmp, golden_path))
    }

    fn write_gz(path: &Path, content: &[u8]) -> Result<()> {
        let file = File::create(path)?;
        let mut encoder = GzEncoder::new(file, Compression::best());
        encoder.write_all(content)?;
        encoder.finish()?;
        Ok(())
    }

    #[test]
    fn test_process_diffs() {
        let content = "identical content\nwith lines\n";
        assert_eq!(Golden::process_diffs(content, content), 0);

        let old = "The quick brown fox\njumps over\nthe lazy dog";
        let new = "The quick red fox\nleaps over\nthe lazy dog";
        assert!(Golden::process_diffs(old, new) > 0);
    }

    #[test]
    fn test_read_plain_and_gz() -> Result<()> {
        let tmp = tempdir()?;
        let plain_path = tmp.path().join("test.txt");
        let gz_path = tmp.path().join("test.txt.gz");

        std::fs::write(&plain_path, "test content")?;
        write_gz(&gz_path, b"compressed")?;

        for (path, expected) in [(&plain_path, "test content"), (&gz_path, "compressed")] {
            let mut reader = Golden::read(path)?;
            let mut content = String::new();
            reader.read_to_string(&mut content)?;
            assert_eq!(content, expected);
        }
        Ok(())
    }

    #[test]
    fn test_verify_plain_and_gz() -> Result<()> {
        let (_tmp, golden_path) = new_golden_dir()?;

        std::fs::write(golden_path.join("match.txt"), "content")?;
        std::fs::write(golden_path.join("empty.txt"), "")?;
        write_gz(golden_path.join("match.txt.gz").as_path(), b"compressed content")?;

        let mut golden = Golden::new_with_mode(&golden_path, GoldenMode::DoNothing)?;
        write!(golden.file("match.txt")?, "content")?;
        drop(golden.file("empty.txt")?);
        write!(golden.file("match.txt.gz")?, "compressed content")?;
        assert!(golden.verify().is_ok());

        let mut golden = Golden::new_with_mode(&golden_path, GoldenMode::DoNothing)?;
        write!(golden.file("match.txt")?, "modified")?;
        assert!(golden.verify().is_err());

        let mut golden = Golden::new_with_mode(&golden_path, GoldenMode::DoNothing)?;
        write!(golden.file("missing.txt")?, "content")?;
        assert!(golden.verify().is_err());

        Ok(())
    }

    #[test]
    fn test_verify_chunk_boundary_difference() -> Result<()> {
        let (_tmp, golden_path) = new_golden_dir()?;

        let mut old_content = String::new();
        for i in 0..80 {
            writeln!(old_content, "Line {i} identical")?;
        }
        let mut new_content = old_content.clone();
        old_content.push_str("old content here");
        new_content.push_str("new content here");

        std::fs::write(golden_path.join("diff_chunk2.txt"), &old_content)?;

        let mut golden = Golden::new_with_mode(&golden_path, GoldenMode::DoNothing)?;
        write!(golden.file("diff_chunk2.txt")?, "{new_content}")?;
        assert!(golden.verify().is_err());
        Ok(())
    }

    #[test]
    fn test_update_creates_files_and_dirs() -> Result<()> {
        let (_tmp, golden_path) = new_golden_dir()?;
        let mut golden = Golden::new_with_mode(&golden_path, GoldenMode::DoNothing)?;

        write!(golden.file("new.txt")?, "new content")?;
        write!(golden.file("subdir/test.txt")?, "nested content")?;
        golden.update()?;

        assert_eq!(std::fs::read_to_string(golden_path.join("new.txt"))?, "new content");
        assert_eq!(std::fs::read_to_string(golden_path.join("subdir/test.txt"))?, "nested content");
        Ok(())
    }

    #[test]
    fn test_update_on_drop() -> Result<()> {
        let (_tmp, golden_path) = new_golden_dir()?;

        std::fs::write(golden_path.join("test.txt"), "old content")?;

        {
            let mut golden = Golden::new_with_mode(&golden_path, GoldenMode::UpdateOnDrop)?;
            let mut file = golden.file("test.txt")?;
            write!(file, "new content")?;
            drop(file);
            // Golden dropped at end of scope and should update the file.
        }

        let updated_content = std::fs::read_to_string(golden_path.join("test.txt"))?;
        assert_eq!(updated_content, "new content");
        Ok(())
    }

    #[test]
    fn test_do_nothing_mode_does_not_update_or_verify() -> Result<()> {
        let (_tmp, golden_path) = new_golden_dir()?;

        std::fs::write(golden_path.join("test.txt"), "original")?;

        {
            let mut golden = Golden::new_with_mode(&golden_path, GoldenMode::DoNothing)?;
            let mut file = golden.file("test.txt")?;
            write!(file, "modified")?;
            drop(file);
            // Drop should not verify or update.
        }

        let updated_content = std::fs::read_to_string(golden_path.join("test.txt"))?;
        assert_eq!(updated_content, "original");
        Ok(())
    }

    #[test]
    fn test_validate_rel_path_cases() {
        let abs = if cfg!(windows) {
            PathBuf::from(r"C:\escape.txt")
        } else {
            PathBuf::from("/escape.txt")
        };

        for p in [Path::new("../escape.txt"), Path::new(""), Path::new("."), &abs] {
            assert!(Golden::validate_rel_path(p).is_err());
        }
        assert!(Golden::validate_rel_path(Path::new("./subdir/test.txt")).is_ok());
    }
}
