use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Write},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Result};
use derive_more::{Deref, DerefMut};
use idm::ser::Indentation;
use indexmap::IndexMap;
use lazy_regex::regex;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub type Section = ((String,), Outline);

/// An outline with a header section.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Outline(pub (IndexMap<String, String>,), pub Vec<Section>);

pub struct ContextIterMut<'a, C> {
    // (context-value, pointer-to-current-item, pointer-past-last-item)
    stack: Vec<(C, *mut Section, *mut Section)>,
    phantom: std::marker::PhantomData<&'a Section>,
}

impl<'a, C: Clone> ContextIterMut<'a, C> {
    fn new(outline: &'a mut Outline, init: C) -> Self {
        let stack = vec![unsafe {
            let a = outline.1.as_mut_ptr();
            let b = a.add(outline.1.len());
            (init, a, b)
        }];
        ContextIterMut {
            stack,
            phantom: std::marker::PhantomData,
        }
    }
}

impl<'a, C: Clone + 'a> Iterator for ContextIterMut<'a, C> {
    type Item = (&'a mut C, &'a mut Section);

    fn next(&mut self) -> Option<Self::Item> {
        // Remove completed ranges.
        while !self.stack.is_empty() {
            let (_, begin, end) = self.stack[self.stack.len() - 1];
            if begin == end {
                self.stack.pop();
            } else {
                break;
            }
        }

        // End iteration if no more content left.
        if self.stack.is_empty() {
            return None;
        }

        let len = self.stack.len();
        // Clone current context object. The clone is pushed for next stack
        // layer and passed as mutable pointer to the iterating context.
        // Context changes will show up in children.
        let ctx = self.stack[len - 1].0.clone();
        let begin = self.stack[len - 1].1;

        // Safety analysis statement: I dunno lol
        unsafe {
            let children = &mut (*begin).1 .1;
            self.stack[len - 1].1 = begin.add(1);
            let a = children.as_mut_ptr();
            let b = a.add(children.len());
            self.stack.push((ctx, a, b));
            let ctx = &mut self.stack[len].0 as *mut C;

            Some((&mut *ctx, &mut *begin))
        }
    }
}

impl Outline {
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Section> {
        self.context_iter_mut(()).map(|x| x.1)
    }

    pub fn context_iter_mut<C: Clone>(
        &mut self,
        init: C,
    ) -> ContextIterMut<'_, C> {
        ContextIterMut::new(self, init)
    }

    /// Get an attribute value deserialized to type.
    pub fn get<'a, T: Deserialize<'a>>(
        &'a self,
        name: &str,
    ) -> Result<Option<T>> {
        let Some(a) = self.0 .0.get(name) else {
            return Ok(None);
        };
        Ok(Some(idm::from_str(a)?))
    }

    pub fn set<T: Serialize>(&mut self, name: &str, value: &T) -> Result<()> {
        self.0 .0.insert(name.to_owned(), idm::to_string(value)?);
        Ok(())
    }
}

impl fmt::Display for Outline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn print(
            f: &mut fmt::Formatter<'_>,
            depth: usize,
            outline: &Outline,
        ) -> fmt::Result {
            for (k, v) in &outline.0 .0 {
                for _ in 0..depth {
                    write!(f, "  ")?;
                }
                write!(f, ":{k}")?;

                if v.chars().any(|c| c == '\n') {
                    // If value is multi-line, write it indented under the key.
                    writeln!(f)?;
                    for line in v.lines() {
                        for _ in 0..(depth + 1) {
                            write!(f, "  ")?;
                        }
                        writeln!(f, "{line}")?;
                    }
                } else {
                    // Otherwise write the value inline.
                    writeln!(f, " {v}")?;
                }
            }

            for ((head,), body) in &outline.1 {
                for _ in 0..depth {
                    write!(f, "  ")?;
                }
                writeln!(f, "{head}")?;
                print(f, depth + 1, body)?;
            }
            Ok(())
        }

        print(f, 0, self)
    }
}

pub type SimpleSection = ((String,), SimpleOutline);

/// An outline without a separately parsed header section.
///
/// Most general form of parsing an IDM document.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SimpleOutline(pub Vec<SimpleSection>);

impl fmt::Display for SimpleOutline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn print(
            f: &mut fmt::Formatter<'_>,
            depth: usize,
            outline: &SimpleOutline,
        ) -> fmt::Result {
            for ((head,), body) in &outline.0 {
                for _ in 0..depth {
                    write!(f, "  ")?;
                }
                writeln!(f, "{head}")?;
                print(f, depth + 1, body)?;
            }
            Ok(())
        }

        print(f, 0, self)
    }
}

/// Context object for a value deserialized from a directory on disk.
#[derive(Clone, Deref, DerefMut, Debug)]
pub struct Collection<T> {
    #[deref]
    #[deref_mut]
    pub inner: T,
    pub style: Indentation,
    path: PathBuf,
    input_files: BTreeSet<PathBuf>,
}

impl<T> Collection<T> {
    /// Read an IDM outline from a directory.
    ///
    /// Subdirectory names get renamed to have trailing slashes so that
    /// subdirectory structure can be preserved when the value is written out
    /// again.
    pub fn load(path: impl AsRef<Path>) -> Result<Self>
    where
        T: DeserializeOwned,
    {
        let mut idm = String::new();
        let mut input_files = BTreeSet::default();
        let mut style = None;

        read_directory(
            &mut idm,
            &mut input_files,
            &mut style,
            "",
            true,
            &path,
        )?;

        Ok(Collection {
            inner: idm::from_str(&idm)?,
            style: style.unwrap_or_default(),
            path: path.as_ref().to_owned(),
            input_files,
        })
    }

    /// Save the collection to disk. Files that were present when the
    /// collection was loaded but are no longer generated from the new value
    /// will be deleted.
    pub fn save(&self) -> Result<BTreeSet<PathBuf>>
    where
        T: Serialize,
    {
        let output_files =
            write_outline_directory(&self.path, self.style, &self.inner)?;

        // Delete files that were present in input but are no longer around
        // with the serialization of the changed(?) inner value.
        for removed_path in self.input_files.difference(&output_files) {
            tidy_delete(&self.path, removed_path)?;
        }

        Ok(output_files)
    }
}

fn read_directory(
    output: &mut String,
    paths: &mut BTreeSet<PathBuf>,
    style: &mut Option<Indentation>,
    prefix: &str,
    outline_mode: bool,
    path: impl AsRef<Path>,
) -> Result<()> {
    let mut elts: Vec<(String, PathBuf)> = Vec::new();

    for e in fs::read_dir(path)? {
        let e = e?;
        let path = e.path();
        let Some(file_name) = path.file_name() else {
            continue;
        };
        let file_name = file_name.to_string_lossy();

        if file_name.starts_with('.') {
            log::debug!("read_directory: skipping dotfile {path:?}");
            continue;
        }

        if !is_valid_filename(&file_name) {
            bail!("read_directory: invalid filename {file_name:?}");
        }

        if path.is_dir() {
            if outline_mode {
                elts.push((format!("{file_name}/"), path.into()));
            } else {
                elts.push((file_name.into(), path.into()));
            }
        } else if path.is_file() {
            match path.extension().map(|a| a.to_string_lossy()) {
                Some(e) if e == "idm" => {
                    elts.push((
                        file_name[..file_name.len() - 4].into(),
                        path.into(),
                    ));
                }
                Some(_) => {
                    // Push other extensions in as-is.
                    elts.push((file_name.into(), path.into()));
                }
                None => {
                    // If they don't, output would assume they had .idm
                    // extensions that got stripped and output with them.
                    bail!("read_directory: file {file_name:?} must have an extension");
                }
            }
        } else {
            // Bail on symlinks.
            bail!("read_directory: unhandled file type {path:?}");
        }
    }

    // Sort into order for outline, make sure names that start with colon come
    // first.
    elts.sort_by(|a, b| {
        (!a.0.starts_with(':'), &a.0).cmp(&(b.0.starts_with(':'), &b.0))
    });

    for (head, path) in elts {
        if path.is_dir() {
            writeln!(output, "{prefix}{head}")?;
            // Recurse into subdirectory.
            read_directory(
                output,
                paths,
                style,
                &format!("{prefix}  "),
                outline_mode,
                path,
            )?;
        } else if path.is_file() {
            paths.insert(path.clone());

            let text = fs::read_to_string(&path)?;

            // It's a single line, just put it right after the headword.
            // This is why file names can't have spaces.
            if !text.contains('\n') {
                writeln!(output, "{prefix}{head} {}", text.trim())?;
                continue;
            }

            // Multiple lines, need to work with indentations etc.
            writeln!(output, "{prefix}{head}")?;
            for line in text.lines() {
                if line.trim().is_empty() {
                    writeln!(output)?;
                    continue;
                }
                write!(output, "{prefix}  ")?;

                if line.starts_with(' ') {
                    match style {
                        None => *style = Some(Indentation::Spaces(2)),
                        Some(Indentation::Tabs) => {
                            bail!("read_directory: inconsistent indentation in {path:?}");
                        }
                        _ => {}
                    }
                }
                if line.starts_with('\t') {
                    match style {
                        None => *style = Some(Indentation::Tabs),
                        Some(Indentation::Spaces(_)) => {
                            bail!("read_directory: inconsistent indentation in {path:?}");
                        }
                        _ => {}
                    }
                }

                let mut ln = &line[..];
                // Turn tab indentation into spaces.
                while ln.starts_with('\t') {
                    write!(output, "  ")?;
                    ln = &ln[1..];
                }
                writeln!(output, "{ln}")?;
            }
        } else {
            // Don't know what this is (symlink?), bail out.
            bail!("read_directory: invalid path {path:?}");
        }
    }

    Ok(())
}

/// Read a structured IDM data directory.
///
/// Subdirectory names are read as is, subdirectory structure will not be
/// preserved if the value is written back into directory.
pub fn read_data_directory<T: DeserializeOwned>(
    path: impl AsRef<Path>,
) -> Result<T> {
    let mut idm = String::new();
    let mut indentation = None;
    read_directory(
        &mut idm,
        &mut Default::default(),
        &mut indentation,
        "",
        false,
        path,
    )?;

    Ok(idm::from_str(&idm)?)
}

fn build_files(
    files: &mut BTreeMap<PathBuf, String>,
    path: impl AsRef<Path>,
    style: Indentation,
    data: &Outline,
) -> Result<()> {
    // Attribute block
    for (key, value) in &data.0 .0 {
        if !is_valid_filename(key) {
            bail!("build_files: bad attribute name {key:?}");
        }
        let path = path.as_ref().join(format!(":{key}.idm"));

        // Ensure value has correct indentation.
        let value = if value.contains('\n') {
            // Only do this with values with newlines, since transmuting to
            // outline will probably mess up the no-trailing-newline semantic
            // difference.
            let value: SimpleOutline = idm::transmute(value)?;
            idm::to_string_styled(style, &value)?
        } else {
            // Newline-less values just get pushed in as is.
            value.into()
        };

        files.insert(path, value);
    }

    // Regular contents
    for ((headline,), data) in &data.1 {
        // You get an empty toplevel section when a file ends with an extra
        // newline. Just ignore these.
        if headline.trim().is_empty() {
            continue;
        }

        let name = if headline.ends_with('/') {
            &headline[0..headline.len() - 1]
        } else {
            &headline[..]
        };
        if !is_valid_filename(name) {
            bail!("build_files: bad headline {headline:?}");
        }

        if headline.ends_with('/') {
            // Create a subdirectory.
            build_files(files, &path.as_ref().join(headline), style, data)?;
            continue;
        }

        let file_name = if headline.contains('.') {
            headline.into()
        } else {
            format!("{headline}.idm")
        };

        let path = path.as_ref().join(file_name);
        files.insert(path, idm::to_string_styled(style, data)?);
    }
    Ok(())
}

fn write_directory(
    path: impl AsRef<Path>,
    style: Indentation,
    data: &Outline,
) -> Result<BTreeSet<PathBuf>> {
    // See that we can build all the contents successfully before deleting
    // anything.
    let mut files = BTreeMap::default();
    build_files(&mut files, path.as_ref(), style, data)?;

    let paths = files.keys().cloned().collect();

    for (path, content) in files {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, content)?;
    }

    Ok(paths)
}

pub fn write_outline_directory<T: Serialize>(
    path: impl AsRef<Path>,
    style: Indentation,
    value: &T,
) -> Result<BTreeSet<PathBuf>> {
    let tree: Outline = idm::transmute(value)?;

    write_directory(path, style, &tree)
}

fn is_valid_filename(s: impl AsRef<str>) -> bool {
    regex!(r"^:?[A-Za-z0-9_-][.A-Za-z0-9_-]*$").is_match(s.as_ref())
}

/// Delete file at `path` and any empty subdirectories between `root` and
/// `path`.
///
/// This is a git-style file delete that deletes the containing subdirectory
/// if it's emptied of files.
fn tidy_delete(root: &Path, mut path: &Path) -> Result<()> {
    fs::remove_file(path)?;
    log::debug!("tidy_delete: Deleted {path:?}");

    loop {
        // Keep going up the subdirectories...
        if let Some(parent) = path.parent() {
            path = parent;
        } else {
            break;
        }

        // ...that are underneath `root`...
        if !path.starts_with(root)
            || path.components().count() <= root.components().count()
        {
            break;
        }

        // ...and deleting them if you can.
        if let Ok(_) = fs::remove_dir(path) {
            log::debug!("tidy_delete: Deleted empty subdirectory {path:?}");
        } else {
            break;
        }
    }

    Ok(())
}
