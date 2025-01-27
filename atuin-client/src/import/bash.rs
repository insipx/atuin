use std::{fs::File, io::Read, path::PathBuf, str};

use async_trait::async_trait;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use directories::UserDirs;
use eyre::{eyre, Result};
use itertools::Itertools;

use super::{get_histpath, unix_byte_lines, Importer, Loader};
use crate::history::History;

#[derive(Debug)]
pub struct Bash {
    bytes: Vec<u8>,
}

fn default_histpath() -> Result<PathBuf> {
    let user_dirs = UserDirs::new().ok_or_else(|| eyre!("could not find user directories"))?;
    let home_dir = user_dirs.home_dir();

    Ok(home_dir.join(".bash_history"))
}

#[async_trait]
impl Importer for Bash {
    const NAME: &'static str = "bash";

    async fn new() -> Result<Self> {
        let mut bytes = Vec::new();
        let path = get_histpath(default_histpath)?;
        let mut f = File::open(path)?;
        f.read_to_end(&mut bytes)?;
        Ok(Self { bytes })
    }

    async fn entries(&mut self) -> Result<usize> {
        let count = unix_byte_lines(&self.bytes)
            .map(LineType::from)
            .filter(|line| matches!(line, LineType::Command(_)))
            .count();
        Ok(count)
    }

    async fn load(self, h: &mut impl Loader) -> Result<()> {
        let lines = unix_byte_lines(&self.bytes)
            .map(LineType::from)
            .filter(|line| !matches!(line, LineType::NotUtf8)) // invalid utf8 are ignored
            .collect_vec();

        let (commands_before_first_timestamp, first_timestamp) = lines
            .iter()
            .enumerate()
            .find_map(|(i, line)| match line {
                LineType::Timestamp(t) => Some((i, *t)),
                _ => None,
            })
            // if no known timestamps, use now as base
            .unwrap_or((lines.len(), Utc::now()));

        // if no timestamp is recorded, then use this increment to set an arbitrary timestamp
        // to preserve ordering
        // this increment is deliberately very small to prevent particularly fast fingers
        // causing ordering issues; it also helps in handling the "here document" syntax,
        // where several lines are recorded in succession without individual timestamps
        let timestamp_increment = Duration::nanoseconds(1);

        // make sure there is a minimum amount of time before the first known timestamp
        // to fit all commands, given the default increment
        let mut next_timestamp =
            first_timestamp - timestamp_increment * commands_before_first_timestamp as i32;

        for line in lines.into_iter() {
            match line {
                LineType::NotUtf8 => unreachable!(), // already filtered
                LineType::Empty => {}                // do nothing
                LineType::Timestamp(t) => {
                    if t < next_timestamp {
                        warn!("Time reversal detected in Bash history! Commands may be ordered incorrectly.");
                    }
                    next_timestamp = t;
                }
                LineType::Command(c) => {
                    let entry = History::new(
                        next_timestamp,
                        c.into(),
                        "unknown".into(),
                        -1,
                        -1,
                        None,
                        None,
                        None,
                    );
                    h.push(entry).await?;
                    next_timestamp += timestamp_increment;
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
enum LineType<'a> {
    NotUtf8,
    /// Can happen when using the "here document" syntax.
    Empty,
    /// A timestamp line start with a '#', followed immediately by an integer
    /// that represents seconds since UNIX epoch.
    Timestamp(DateTime<Utc>),
    /// Anything else.
    Command(&'a str),
}
impl<'a> From<&'a [u8]> for LineType<'a> {
    fn from(bytes: &'a [u8]) -> Self {
        let Ok(line) = str::from_utf8(bytes) else {
            return LineType::NotUtf8;
        };
        if line.is_empty() {
            return LineType::Empty;
        }
        let parsed = match try_parse_line_as_timestamp(line) {
            Some(time) => LineType::Timestamp(time),
            None => LineType::Command(line),
        };
        parsed
    }
}

fn try_parse_line_as_timestamp(line: &str) -> Option<DateTime<Utc>> {
    let seconds = line.strip_prefix('#')?.parse().ok()?;
    let time = NaiveDateTime::from_timestamp(seconds, 0);
    Some(DateTime::from_utc(time, Utc))
}

#[cfg(test)]
mod test {
    use std::cmp::Ordering;

    use itertools::{assert_equal, Itertools};

    use crate::import::{tests::TestLoader, Importer};

    use super::Bash;

    #[tokio::test]
    async fn parse_no_timestamps() {
        let bytes = r"cargo install atuin
cargo update
cargo :b̷i̶t̴r̵o̴t̴ ̵i̷s̴ ̷r̶e̵a̸l̷
"
        .as_bytes()
        .to_owned();

        let mut bash = Bash { bytes };
        assert_eq!(bash.entries().await.unwrap(), 3);

        let mut loader = TestLoader::default();
        bash.load(&mut loader).await.unwrap();

        assert_equal(
            loader.buf.iter().map(|h| h.command.as_str()),
            [
                "cargo install atuin",
                "cargo update",
                "cargo :b̷i̶t̴r̵o̴t̴ ̵i̷s̴ ̷r̶e̵a̸l̷",
            ],
        );
        assert!(is_strictly_sorted(loader.buf.iter().map(|h| h.timestamp)))
    }

    #[tokio::test]
    async fn parse_with_timestamps() {
        let bytes = b"#1672918999
git reset
#1672919006
git clean -dxf
#1672919020
cd ../
"
        .to_vec();

        let mut bash = Bash { bytes };
        assert_eq!(bash.entries().await.unwrap(), 3);

        let mut loader = TestLoader::default();
        bash.load(&mut loader).await.unwrap();

        assert_equal(
            loader.buf.iter().map(|h| h.command.as_str()),
            ["git reset", "git clean -dxf", "cd ../"],
        );
        assert_equal(
            loader.buf.iter().map(|h| h.timestamp.timestamp()),
            [1672918999, 1672919006, 1672919020],
        )
    }

    #[tokio::test]
    async fn parse_with_partial_timestamps() {
        let bytes = b"git reset
#1672919006
git clean -dxf
cd ../
"
        .to_vec();

        let mut bash = Bash { bytes };
        assert_eq!(bash.entries().await.unwrap(), 3);

        let mut loader = TestLoader::default();
        bash.load(&mut loader).await.unwrap();

        assert_equal(
            loader.buf.iter().map(|h| h.command.as_str()),
            ["git reset", "git clean -dxf", "cd ../"],
        );
        assert!(is_strictly_sorted(loader.buf.iter().map(|h| h.timestamp)))
    }

    fn is_strictly_sorted<T>(iter: impl IntoIterator<Item = T>) -> bool
    where
        T: Clone + PartialOrd,
    {
        iter.into_iter()
            .tuple_windows()
            .all(|(a, b)| matches!(a.partial_cmp(&b), Some(Ordering::Less)))
    }
}
