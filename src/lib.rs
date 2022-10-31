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
    clippy::match_on_vec_items,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::module_name_repetitions,
    clippy::similar_names,
    clippy::struct_excessive_bools,
    clippy::too_many_lines,
    clippy::unreadable_literal
)]

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::{env, thread};

use colored::Colorize;
use dissimilar::{diff, Chunk};
use eyre::{eyre, Result};
use flate2::bufread::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use tempfile::{tempdir, TempDir};

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
#[derive(Debug)]
pub struct Golden {
    golden: PathBuf,
    tmp: TempDir,
    paths: Vec<PathBuf>,
}

const BYTE_LIMIT: u64 = 1024;

impl Golden {
    pub fn new(p: impl AsRef<Path>) -> Result<Self> {
        Ok(Self { golden: p.as_ref().to_path_buf(), tmp: tempdir()?, paths: Vec::new() })
    }

    pub fn file(&mut self, p: impl AsRef<Path>) -> Result<Box<dyn Write>> {
        self.write_tmp(p.as_ref())
    }

    fn write_tmp(&mut self, p: &Path) -> Result<Box<dyn Write>> {
        self.paths.push(p.to_owned());
        let f = BufWriter::new(File::create(self.tmp.path().join(p))?);
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
        let mut okay_count = 0;
        let mut old = Cursor::new(old);
        let mut new = Cursor::new(new);
        for chunk in &chunks {
            match chunk {
                Chunk::Equal(s) => {
                    old.advance(s.len(), CursorOp::Equal, true);
                    new.advance(s.len(), CursorOp::Equal, false); // Don't double print for equal chunks.
                    okay_count += 1;
                }
                Chunk::Delete(s) => old.advance(s.len(), CursorOp::Delete, true),
                Chunk::Insert(s) => new.advance(s.len(), CursorOp::Insert, false),
            }
        }
        let num = chunks.len() - okay_count;
        if num != 0 {
            println!();
        }
        num
    }

    fn verify(&self) -> Result<()> {
        for p in &self.paths {
            let mut golden = Self::read(&self.golden.join(p))?;
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

    fn update(&self) -> Result<()> {
        for p in &self.paths {
            fs::copy(self.tmp.path().join(p), self.golden.join(p))?;
        }
        Ok(())
    }
}

impl Drop for Golden {
    fn drop(&mut self) {
        if thread::panicking() {
            return;
        }
        let r = env::var("UPDATE_GOLDEN");
        if r.is_ok() && r.unwrap() == "1" {
            self.update().expect("could not update golden files");
        } else {
            self.verify().expect("could not verify golden files");
        }
    }
}
